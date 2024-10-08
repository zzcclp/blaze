// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Execution plan for reading Parquet files

use std::{any::Any, fmt, fmt::Formatter, ops::Range, pin::Pin, sync::Arc};

use arrow::datatypes::SchemaRef;
use blaze_jni_bridge::{
    conf, conf::BooleanConf, jni_call_static, jni_new_global_ref, jni_new_string,
};
use bytes::Bytes;
use datafusion::{
    datasource::physical_plan::{
        parquet::{page_filter::PagePruningAccessPlanFilter, ParquetOpener},
        FileMeta, FileScanConfig, FileStream, OnError, ParquetFileMetrics,
        ParquetFileReaderFactory,
    },
    error::Result,
    execution::context::TaskContext,
    parquet::{
        arrow::async_reader::{fetch_parquet_metadata, AsyncFileReader},
        errors::ParquetError,
        file::metadata::ParquetMetaData,
    },
    physical_expr::EquivalenceProperties,
    physical_optimizer::pruning::PruningPredicate,
    physical_plan::{
        metrics::{
            BaselineMetrics, ExecutionPlanMetricsSet, MetricBuilder, MetricValue, MetricsSet, Time,
        },
        stream::RecordBatchStreamAdapter,
        DisplayAs, DisplayFormatType, ExecutionMode, ExecutionPlan, Metric, Partitioning,
        PhysicalExpr, PlanProperties, RecordBatchStream, SendableRecordBatchStream, Statistics,
    },
};
use datafusion_ext_commons::{batch_size, hadoop_fs::FsProvider};
use fmt::Debug;
use futures::{future::BoxFuture, stream::once, FutureExt, StreamExt, TryStreamExt};
use object_store::ObjectMeta;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use crate::{
    common::{internal_file_reader::InternalFileReader, output::TaskOutputter},
    scan::BlazeSchemaAdapterFactory,
};

/// Execution plan for scanning one or more Parquet partitions
#[derive(Debug, Clone)]
pub struct ParquetExec {
    fs_resource_id: String,
    base_config: FileScanConfig,
    projected_statistics: Statistics,
    projected_schema: SchemaRef,
    metrics: ExecutionPlanMetricsSet,
    predicate: Option<Arc<dyn PhysicalExpr>>,
    pruning_predicate: Option<Arc<PruningPredicate>>,
    page_pruning_predicate: Option<Arc<PagePruningAccessPlanFilter>>,
    props: OnceCell<PlanProperties>,
}

impl ParquetExec {
    /// Create a new Parquet reader execution plan provided file list and
    /// schema.
    pub fn new(
        base_config: FileScanConfig,
        fs_resource_id: String,
        predicate: Option<Arc<dyn PhysicalExpr>>,
    ) -> Self {
        let metrics = ExecutionPlanMetricsSet::new();
        let predicate_creation_errors =
            MetricBuilder::new(&metrics).global_counter("num_predicate_creation_errors");

        let file_schema = &base_config.file_schema;
        let pruning_predicate = predicate
            .clone()
            .and_then(|predicate_expr| {
                match PruningPredicate::try_new(predicate_expr, file_schema.clone()) {
                    Ok(pruning_predicate) => Some(Arc::new(pruning_predicate)),
                    Err(e) => {
                        log::warn!("Could not create pruning predicate: {e}");
                        predicate_creation_errors.add(1);
                        None
                    }
                }
            })
            .filter(|p| !p.always_true());

        let page_pruning_predicate = predicate
            .as_ref()
            .map(|p| Arc::new(PagePruningAccessPlanFilter::new(p, file_schema.clone())));

        let (projected_schema, projected_statistics, _projected_output_ordering) =
            base_config.project();

        Self {
            fs_resource_id,
            base_config,
            projected_schema,
            projected_statistics,
            metrics,
            predicate,
            pruning_predicate,
            page_pruning_predicate,
            props: OnceCell::new(),
        }
    }
}

impl DisplayAs for ParquetExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        let limit = self.base_config.limit;
        let file_group = self
            .base_config
            .file_groups
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();

        write!(
            f,
            "ParquetExec: limit={:?}, file_group={:?}, predicate={}",
            limit,
            file_group,
            self.pruning_predicate
                .as_ref()
                .map(|pre| format!("{}", pre.predicate_expr()))
                .unwrap_or(format!("<empty>")),
        )
    }
}

impl ExecutionPlan for ParquetExec {
    fn name(&self) -> &str {
        "ParquetExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.projected_schema)
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn properties(&self) -> &PlanProperties {
        self.props.get_or_init(|| {
            PlanProperties::new(
                EquivalenceProperties::new(self.schema()),
                Partitioning::UnknownPartitioning(self.base_config.file_groups.len()),
                ExecutionMode::Bounded,
            )
        })
    }

    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        partition_index: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition_index);
        let elapsed_compute = baseline_metrics.elapsed_compute().clone();
        let _timer = elapsed_compute.timer();

        let io_time = Time::default();
        let io_time_metric = Arc::new(Metric::new(
            MetricValue::Time {
                name: "io_time".into(),
                time: io_time.clone(),
            },
            Some(partition_index),
        ));
        self.metrics.register(io_time_metric);

        // get fs object from jni bridge resource
        let resource_id = jni_new_string!(&self.fs_resource_id)?;
        let fs = jni_call_static!(JniBridge.getResource(resource_id.as_obj()) -> JObject)?;
        let fs_provider = Arc::new(FsProvider::new(jni_new_global_ref!(fs.as_obj())?, &io_time));

        let schema_adapter_factory = Arc::new(BlazeSchemaAdapterFactory);
        let projection = match self.base_config.file_column_projection_indices() {
            Some(proj) => proj,
            None => (0..self.base_config.file_schema.fields().len()).collect(),
        };

        let page_filtering_enabled = conf::PARQUET_ENABLE_PAGE_FILTERING.value()?;
        let bloom_filter_enabled = conf::PARQUET_ENABLE_BLOOM_FILTER.value()?;

        let opener = ParquetOpener {
            partition_index,
            projection: Arc::from(projection),
            batch_size: batch_size(),
            limit: self.base_config.limit,
            predicate: self.predicate.clone(),
            pruning_predicate: self.pruning_predicate.clone(),
            page_pruning_predicate: self.page_pruning_predicate.clone(),
            table_schema: self.base_config.file_schema.clone(),
            metadata_size_hint: None,
            metrics: self.metrics.clone(),
            parquet_file_reader_factory: Arc::new(FsReaderFactory::new(fs_provider)),
            pushdown_filters: page_filtering_enabled,
            reorder_filters: page_filtering_enabled,
            enable_page_index: page_filtering_enabled,
            enable_bloom_filter: bloom_filter_enabled,
            schema_adapter_factory,
        };

        let mut file_stream =
            FileStream::new(&self.base_config, partition_index, opener, &self.metrics)?;
        if conf::IGNORE_CORRUPTED_FILES.value()? {
            file_stream = file_stream.with_on_error(OnError::Skip);
        }
        let stream = Box::pin(file_stream);
        let timed_stream = Box::pin(RecordBatchStreamAdapter::new(
            self.schema(),
            once(execute_parquet_scan(context, stream, baseline_metrics)).try_flatten(),
        ));
        Ok(timed_stream)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Result<Statistics> {
        Ok(self.projected_statistics.clone())
    }
}

async fn execute_parquet_scan(
    context: Arc<TaskContext>,
    mut stream: Pin<Box<FileStream<ParquetOpener>>>,
    baseline_metrics: BaselineMetrics,
) -> Result<SendableRecordBatchStream> {
    let schema = stream.schema();
    context.output_with_sender("ParquetScan", schema, move |sender| async move {
        sender.exclude_time(baseline_metrics.elapsed_compute());

        let _timer = baseline_metrics.elapsed_compute().timer();
        while let Some(batch) = stream.next().await.transpose()? {
            sender.send(Ok(batch)).await;
        }
        Ok(())
    })
}

#[derive(Clone)]
pub struct FsReaderFactory {
    fs_provider: Arc<FsProvider>,
}

impl FsReaderFactory {
    pub fn new(fs_provider: Arc<FsProvider>) -> Self {
        Self { fs_provider }
    }
}

impl Debug for FsReaderFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "FsReaderFactory")
    }
}

impl ParquetFileReaderFactory for FsReaderFactory {
    fn create_reader(
        &self,
        partition_index: usize,
        file_meta: FileMeta,
        _metadata_size_hint: Option<usize>,
        metrics: &ExecutionPlanMetricsSet,
    ) -> Result<Box<dyn AsyncFileReader + Send>> {
        let internal_reader = Arc::new(InternalFileReader::try_new(
            self.fs_provider.clone(),
            file_meta.object_meta.clone(),
        )?);
        let reader = ParquetFileReaderRef(Arc::new(ParquetFileReader {
            internal_reader,
            metrics: ParquetFileMetrics::new(
                partition_index,
                file_meta
                    .object_meta
                    .location
                    .filename()
                    .unwrap_or("__default_filename__"),
                metrics,
            ),
        }));
        Ok(Box::new(reader))
    }
}

struct ParquetFileReader {
    internal_reader: Arc<InternalFileReader>,
    metrics: ParquetFileMetrics,
}

#[derive(Clone)]
struct ParquetFileReaderRef(Arc<ParquetFileReader>);

impl ParquetFileReader {
    fn get_meta(&self) -> ObjectMeta {
        self.internal_reader.get_meta()
    }

    fn get_internal_reader(&self) -> Arc<InternalFileReader> {
        self.internal_reader.clone()
    }
}

impl AsyncFileReader for ParquetFileReaderRef {
    fn get_bytes(
        &mut self,
        range: Range<usize>,
    ) -> BoxFuture<'_, datafusion::parquet::errors::Result<Bytes>> {
        let inner = self.0.clone();
        inner.metrics.bytes_scanned.add(range.end - range.start);
        async move {
            tokio::task::spawn_blocking(move || {
                inner
                    .get_internal_reader()
                    .read_fully(range)
                    .map_err(|e| ParquetError::External(Box::new(e)))
            })
            .await
            .expect("tokio spawn_blocking error")
        }
        .boxed()
    }

    fn get_metadata(
        &mut self,
    ) -> BoxFuture<'_, datafusion::parquet::errors::Result<Arc<ParquetMetaData>>> {
        const METADATA_CACHE_SIZE: usize = 5; // TODO: make it configurable

        type ParquetMetaDataSlot = tokio::sync::OnceCell<Arc<ParquetMetaData>>;
        type ParquetMetaDataCacheTable = Vec<(ObjectMeta, ParquetMetaDataSlot)>;
        static METADATA_CACHE: OnceCell<Mutex<ParquetMetaDataCacheTable>> = OnceCell::new();

        let inner = self.0.clone();
        let meta_size = inner.get_meta().size;
        let size_hint = None;
        let cache_slot = (move || {
            let mut metadata_cache = METADATA_CACHE.get_or_init(|| Mutex::new(Vec::new())).lock();

            // find existed cache slot
            for (cache_meta, cache_slot) in metadata_cache.iter() {
                if cache_meta.location == self.0.get_meta().location {
                    return cache_slot.clone();
                }
            }

            // reserve a new cache slot
            if metadata_cache.len() >= METADATA_CACHE_SIZE {
                metadata_cache.remove(0); // remove eldest
            }
            let cache_slot = ParquetMetaDataSlot::default();
            metadata_cache.push((self.0.get_meta().clone(), cache_slot.clone()));
            cache_slot
        })();

        // fetch metadata from file and update to cache
        async move {
            cache_slot
                .get_or_try_init(move || async move {
                    fetch_parquet_metadata(
                        move |range| {
                            let inner = inner.clone();
                            inner.metrics.bytes_scanned.add(range.end - range.start);
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    inner
                                        .get_internal_reader()
                                        .read_fully(range)
                                        .map_err(|e| ParquetError::External(Box::new(e)))
                                })
                                .await
                                .expect("tokio spawn_blocking error")
                            }
                        },
                        meta_size,
                        size_hint,
                    )
                    .await
                    .map(|parquet_metadata| Arc::new(parquet_metadata))
                })
                .map(|parquet_metadata| parquet_metadata.cloned())
                .await
        }
        .boxed()
    }
}
