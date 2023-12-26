use std::{
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Instant,
};

use async_trait::async_trait;
use concurrent_queue::ConcurrentQueue;
use dt_meta::{
    adaptor::{mysql_col_value_convertor::MysqlColValueConvertor, sqlx_ext::SqlxMysqlExt},
    col_value::ColValue,
    dt_data::DtItem,
    mysql::{
        mysql_col_type::MysqlColType, mysql_meta_manager::MysqlMetaManager,
        mysql_tb_meta::MysqlTbMeta,
    },
    position::Position,
    row_data::RowData,
};
use futures::TryStreamExt;

use sqlx::{MySql, Pool};

use dt_common::{config::config_enums::DbType, error::Error, log_info, monitor::monitor::Monitor};

use crate::{
    extractor::{base_extractor::BaseExtractor, snapshot_resumer::SnapshotResumer},
    rdb_router::RdbRouter,
    Extractor,
};

pub struct MysqlSnapshotExtractor {
    pub conn_pool: Pool<MySql>,
    pub meta_manager: MysqlMetaManager,
    pub resumer: SnapshotResumer,
    pub buffer: Arc<ConcurrentQueue<DtItem>>,
    pub slice_size: usize,
    pub sample_interval: usize,
    pub db: String,
    pub tb: String,
    pub shut_down: Arc<AtomicBool>,
    pub monitor: Arc<Mutex<Monitor>>,
    pub router: RdbRouter,
}

#[async_trait]
impl Extractor for MysqlSnapshotExtractor {
    async fn extract(&mut self) -> Result<(), Error> {
        log_info!(
            "MysqlSnapshotExtractor starts, schema: `{}`, tb: `{}`, slice_size: {}",
            self.db,
            self.tb,
            self.slice_size
        );
        self.extract_internal().await
    }

    async fn close(&mut self) -> Result<(), Error> {
        if self.conn_pool.is_closed() {
            return Ok(());
        }
        self.conn_pool.close().await;
        Ok(())
    }
}

impl MysqlSnapshotExtractor {
    async fn extract_internal(&mut self) -> Result<(), Error> {
        let tb_meta = self
            .meta_manager
            .get_tb_meta(&self.db, &self.tb)
            .await?
            .to_owned();

        if let Some(order_col) = &tb_meta.basic.order_col {
            let order_col_type = tb_meta.col_type_map.get(order_col).unwrap();

            let resume_value =
                if let Some(value) = self.resumer.get_resume_value(&self.db, &self.tb, order_col) {
                    MysqlColValueConvertor::from_str(order_col_type, &value).unwrap()
                } else {
                    ColValue::None
                };

            self.extract_by_slices(&tb_meta, order_col, order_col_type, resume_value)
                .await?;
        } else {
            self.extract_all(&tb_meta).await?;
        }

        BaseExtractor::wait_task_finish(self.buffer.as_ref(), self.shut_down.as_ref()).await
    }

    async fn extract_all(&mut self, tb_meta: &MysqlTbMeta) -> Result<(), Error> {
        let mut last_monitored_time = Instant::now();
        let monitor_count_window = self.monitor.lock().unwrap().count_window;
        let monitor_time_window_secs = self.monitor.lock().unwrap().time_window_secs as u64;
        let mut monitored_count = 0;
        let mut extracted_count = 0;

        log_info!(
            "start extracting data from `{}`.`{}` without slices",
            self.db,
            self.tb
        );

        let sql = format!("SELECT * FROM `{}`.`{}`", self.db, self.tb);
        let mut rows = sqlx::query(&sql).fetch(&self.conn_pool);
        while let Some(row) = rows.try_next().await.unwrap() {
            let row_data = RowData::from_mysql_row(&row, tb_meta);
            BaseExtractor::push_row(
                self.buffer.as_ref(),
                row_data,
                Position::None,
                Some(&self.router),
            )
            .await
            .unwrap();
            extracted_count += 1;

            (last_monitored_time, monitored_count) = BaseExtractor::update_monitor(
                &mut self.monitor,
                extracted_count,
                monitored_count,
                monitor_count_window,
                monitor_time_window_secs,
                last_monitored_time,
            );
        }

        log_info!(
            "end extracting data from `{}`.`{}`, all count: {}",
            self.db,
            self.tb,
            extracted_count
        );
        Ok(())
    }

    async fn extract_by_slices(
        &mut self,
        tb_meta: &MysqlTbMeta,
        order_col: &str,
        order_col_type: &MysqlColType,
        resume_value: ColValue,
    ) -> Result<(), Error> {
        let mut last_monitored_time = Instant::now();
        let monitor_count_window = self.monitor.lock().unwrap().count_window;
        let monitor_time_window_secs = self.monitor.lock().unwrap().time_window_secs as u64;
        let mut monitored_count = 0;
        let mut extracted_count = 0;

        log_info!(
            "start extracting data from `{}`.`{}` by slices",
            self.db,
            self.tb
        );

        let mut start_value = resume_value;
        let sql1 = format!(
            "SELECT * FROM `{}`.`{}` ORDER BY `{}` ASC LIMIT {}",
            self.db, self.tb, order_col, self.slice_size
        );
        let sql2 = format!(
            "SELECT * FROM `{}`.`{}` WHERE `{}` > ? ORDER BY `{}` ASC LIMIT {}",
            self.db, self.tb, order_col, order_col, self.slice_size
        );

        loop {
            let start_value_for_bind = start_value.clone();
            let query = if let ColValue::None = start_value {
                sqlx::query(&sql1)
            } else {
                sqlx::query(&sql2).bind_col_value(Some(&start_value_for_bind), order_col_type)
            };

            let mut rows = query.fetch(&self.conn_pool);
            let mut slice_count = 0usize;
            while let Some(row) = rows.try_next().await.unwrap() {
                start_value =
                    MysqlColValueConvertor::from_query(&row, order_col, order_col_type).unwrap();
                extracted_count += 1;
                slice_count += 1;
                // sampling may be used in check scenario
                if extracted_count % self.sample_interval != 0 {
                    continue;
                }

                let row_data = RowData::from_mysql_row(&row, tb_meta);
                let position = if let Some(value) = start_value.to_option_string() {
                    Position::RdbSnapshot {
                        db_type: DbType::Mysql.to_string(),
                        schema: self.db.clone(),
                        tb: self.tb.clone(),
                        order_col: order_col.into(),
                        value,
                    }
                } else {
                    Position::None
                };
                BaseExtractor::push_row(
                    self.buffer.as_ref(),
                    row_data,
                    position,
                    Some(&self.router),
                )
                .await
                .unwrap();

                (last_monitored_time, monitored_count) = BaseExtractor::update_monitor(
                    &mut self.monitor,
                    extracted_count,
                    monitored_count,
                    monitor_count_window,
                    monitor_time_window_secs,
                    last_monitored_time,
                );
            }

            // all data extracted
            if slice_count < self.slice_size {
                break;
            }
        }

        log_info!(
            "end extracting data from `{}`.`{}`, all count: {}",
            self.db,
            self.tb,
            extracted_count
        );
        Ok(())
    }
}
