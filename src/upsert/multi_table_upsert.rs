use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use log::{error, info, trace, warn};
use native_tls::{Certificate, TlsConnector};
use postgres_native_tls::MakeTlsConnector;
use support::DataHolder;
use tokio::{sync::mpsc::{self, Receiver, Sender}, task::JoinHandle};
use tokio_postgres::{Client, Error, NoTls};
use tokio_util::sync::CancellationToken;

#[cfg(all(unix, feature = "unix-signals"))]
use crate::shutdown_service;

use crate::{builder::support::{MultiTableUpsertQueryHolder, MultiTableSingleQueryHolder}, introduce_lag, split_vec};

pub mod support;

use super::Upsert;

#[async_trait]
pub trait MultiTableUpsert<T>: Send + Sync + Upsert<T>
where
    T: Clone + Send + Sync,
{
    fn table(&self) -> String;
    fn tables() -> Vec<String>;
}

#[derive(Debug)]
struct UpsertData<T> where T: MultiTableUpsert<T> + Clone + Send {
    pub tx: Sender<Vec<T>>,
    pub join_handler: JoinHandle<u8>,
    pub id: i64,
    pub type_: usize
}

impl<T> UpsertData<T> where T: MultiTableUpsert<T> + Clone + Send {
    pub fn new(tx: Sender<Vec<T>>, join_handler: JoinHandle<u8>, id: i64, type_: usize) -> Self {
        Self {
            tx,
            join_handler,
            id,
            type_
        }
    }
}



#[derive(Default, Clone)]
pub struct MultiTableUpsertQuickStream {
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) max_con_count: usize,
    pub(crate) buffer_size: usize,
    pub(crate) single_digits: usize,
    pub(crate) tens: usize,
    pub(crate) hundreds: usize,
    pub(crate) db_config: tokio_postgres::Config,
    pub(crate) tls: Option<Certificate>,
    pub(crate) queries: MultiTableUpsertQueryHolder,
    pub(crate) max_records_per_cycle_batch: usize, //a batch = introduced_lag_cycles
    pub(crate) introduced_lag_cycles: usize,
    pub(crate) introduced_lag_in_millies: u64,
    pub(crate) connection_creation_threshold: f64,
    pub(crate) name: String,
    pub(crate) print_con_config: bool
}


impl MultiTableUpsertQuickStream {
    pub async fn run<T>(&self, mut rx: Receiver<Vec<T>>) where T: MultiTableUpsert<T> + Clone + Send + 'static {

        info!("{}: upsert quick stream is starting", self.name);
        info!("{}: testing database connections", self.name);
        let _client = self.get_db_client().await;
        drop(_client);
        info!("{}: database sucsessfully connected", self.name);
        let mut tx_count = 0;

        trace!("{}: initiating senders", self.name);
        let mut senders = self.init_senders::<T>(&mut tx_count);
        trace!("{}: inititating senders complete", self.name);

        #[cfg(all(unix, feature = "unix-signals"))]
        let cancellation_token = self.cancellation_token.clone();

        #[cfg(all(unix, feature = "unix-signals"))]
        let unix_shutdown_service = tokio::spawn(async move {
            shutdown_service::shutdown_unix(cancellation_token).await;
            3u8
        });

        #[cfg(all(windows, feature = "windows-signals"))]
        let cancellation_token = self.cancellation_token.clone();

        #[cfg(all(windows, feature = "windows-signals"))]
        match ctrlc::try_set_handler(move || {
            cancellation_token.cancel();
        }) {
            Ok(_) => trace!("{}: ctrlc handler set", self.name),
            Err(_) => error!("{}: ctrlc handler failed to set", self.name)
        };
        
        info!("{}: main channel receiver starting", self.name);
        'outer: loop {
            tokio::select! {
                Some(data) = rx.recv() => {
                    self.process_received(data, &mut senders, &mut tx_count, &mut rx).await;
                }
                _ = self.cancellation_token.cancelled() => {
                    info!("{}: cancellation token received. shutting down upsert quick stream", self.name);
                    break 'outer;
                }
            }
        }

        for (type_, sender) in senders {
            info!("{}: shutting down senders of type {}", self.name, type_);
            for upsert_data in sender {
                match upsert_data.join_handler.await {
                    Ok(_) => trace!("{}: sender {}:{} shutdown", self.name, type_, upsert_data.id),
                    Err(error) => error!("{}: sender {}:{} shutdown failed with error: {}", self.name, type_, upsert_data.id, error),
                };
            }
            info!("{}: senders of type {} shutdown complete", self.name, type_);
        }

        #[cfg(all(unix, feature = "unix-signals"))]
        match unix_shutdown_service.await {
            Ok(_) => info!("{}: upsert quick stream shutdown service complete", self.name),
            Err(error) => error!("{}: upsert quick stream shutdown service failed with error: {}", self.name, error)
        }

        info!("{}: upsert quick stream shutdown complete", self.name);
    }

    async fn process_received<T>(&self, data: Vec<T>,mut senders: &mut HashMap<usize, Vec<UpsertData<T>>>, mut tx_count: &mut i64, rx: &mut Receiver<Vec<T>>) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        let mut data_holder = DataHolder::<T>::default();
        trace!("{}: data received. Adding data to a data holder", self.name);
        let data_ready_to_process = data_holder.add_all(data, self.max_records_per_cycle_batch);
        if data_ready_to_process.len() > 0 {
            trace!("{}: ready to process data available. proceding for ingestion one table at a time", self.name);
            for (table, data) in data_ready_to_process {
                trace!("{}: data count: {} exceeds max records per cycle batch: {}. proceeding for ingestion to table: {}", self.name, data.len(), self.max_records_per_cycle_batch, table);
                self.send_processed(data, table, &mut senders, &mut tx_count).await;
            }
        } else if data_holder.len() > 0 {

            trace!("{}: starting lag cycles", self.name);
            let mut introduced_lag_cycles = 0;
            'inner: loop {
                match rx.try_recv() {
                    Ok(more_data) => {
                        trace!("{}: more data received. Adding data to a data holder", self.name);
                        let data_ready_to_process = data_holder.add_all(more_data, self.max_records_per_cycle_batch);

                        trace!("{}: ready to process data available. proceding for ingestion one table at a time", self.name);
                        for (table, data) in data_ready_to_process {
                            trace!("{}: data count: {} exceeds max records per cycle batch: {}. breaking the lag cycle and proceesing for ingestion", self.name, data.len(), self.max_records_per_cycle_batch);
                            self.send_processed(data, table, &mut senders, &mut tx_count).await;
                        }

                        if data_holder.len() == 0 {
                            trace!("{}: no more data to process. breaking the lag cycle", self.name);
                            break 'inner;
                        }
                    },
                    Err(_) => {
                        trace!("{}: no data received. data count: {}", self.name, data_holder.len());
                        introduced_lag_cycles += 1;

                        trace!("{}: lag cycles: {}", self.name, introduced_lag_cycles);
                        // greater than is used allowing 0 lag cycles
                        if introduced_lag_cycles > self.introduced_lag_cycles {
                            trace!("{}: lag cycles: {} exceeds or reached max introduced lag cycles. data count : {}. proceeding for ingestion.", self.name, self.introduced_lag_cycles, data_holder.len());
                            break 'inner;
                        } else {
                            trace!("{}: introducing lag", self.name);
                            introduce_lag(self.introduced_lag_in_millies).await;
                            trace!("{}: introduced lag successfull", self.name);
                        }
                    },
                }
            };

            trace!("{}: lag cycles complete. Getting all data from data holder to process", self.name);
            let all_data = data_holder.get_all();
        
            for (table, data) in all_data {
                trace!("{}: data count: {} exceeds max records per cycle batch: {}. proceeding for ingestion to table: {}", self.name, data.len(), self.max_records_per_cycle_batch, table);
                self.send_processed(data, table, &mut senders, &mut tx_count).await;
            }
            
        }

        self.rebalance_senders(&mut senders, &mut tx_count);
    }

    async fn send_processed<T>(&self, data: Vec<T>, table: String, senders: &mut HashMap<usize, Vec<UpsertData<T>>>, tx_count: &mut i64 ) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        trace!("{}: data count: {} exceeds max records per cycle batch: {}. proceeding for ingestion to table: {}", self.name, data.len(), self.max_records_per_cycle_batch, table);

        trace!("{}: splitting vectors for batch ingestion for table: {}", self.name, table);
        let vec_data = split_vec(data);
        trace!("{}: splitting vectors complete. batch count: {} for table: {}", self.name, vec_data.len(), table);

        trace!("{}: data ingestion starting for batches of table: {}", self.name, table);
        self.push_to_handle(senders, vec_data.to_owned(), tx_count).await;
        trace!("{}: data pushed for ingestion for table: {}", self.name, table);
    }

    async fn get_db_client(&self) -> Client {
        trace!("{}: creating database client", self.name);
        let config = self.db_config.to_owned();

        match &self.tls {
            Some(tls) => {
                trace!("{}: tls is enabled", self.name);
                trace!("{}: creating tls connector", self.name);
                let connector = TlsConnector::builder()
                    .add_root_certificate(tls.clone())
                    .build()
                    .unwrap();

                let tls = MakeTlsConnector::new(connector);

                trace!("{}: creating tls connector success", self.name);

                trace!("{}: establishing database connection with tls", self.name);
                let (client, connection) = match config
                    .connect(tls)
                    .await {
                    Ok(cnc) => cnc,
                    Err(error) => panic!("error occured during database client establishment with tls, error : {}", error)
                };
                trace!("{}: establishing database connection with tls success", self.name);
        
                trace!("{}: creating thread to hold the database connection with tls", self.name);
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        eprintln!("connection failed with error : {}", error)
                    }
                });
        
                trace!("{}: creating database client with tls success, returning client", self.name);
                client                
            },
            None => {
                trace!("{}: tls is dissabled", self.name);

                trace!("{}: establishing database connection", self.name);
                let (client, connection) = match config
                    .connect(NoTls)
                    .await {
                    Ok(cnc) => cnc,
                    Err(error) => panic!("error occured during database client establishment, error : {}", error)
                };
                trace!("{}: establishing database connection success", self.name);
        
                trace!("{}: creating thread to hold the database connection", self.name);
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        eprintln!("connection failed with error : {}", error)
                    }
                });
                trace!("{}: creating thread to hold the database connection success", self.name);
        
                trace!("{}: creating database client success, returning client", self.name);
                client
            },
        }
    }

    async fn process_n<T>(&self, multi_table_single_queries: MultiTableSingleQueryHolder, mut rx: Receiver<Vec<T>>, thread_id: i64, n: usize) -> Result<(), Error>  where T: MultiTableUpsert<T> + Clone + Send + 'static {
        info!("{}:{}:{}: starting data ingestor", self.name, n, thread_id);

        info!("{}:{}:{}: creating database client", self.name, n, thread_id);
        let client = self.get_db_client().await;
        info!("{}:{}:{}: creating database client success", self.name, n, thread_id);

        info!("{}:{}:{}: preparing queries and creating statement map", self.name, n, thread_id);
        let statement_map = multi_table_single_queries.prepare(&client).await;
        info!("{}:{}:{}: queries prepared and created statement map successfully", self.name, n, thread_id);

        info!("{}:{}:{}: data ingestor channel receiver starting", self.name, n, thread_id);
        'inner: loop {
            tokio::select! {
                Some(data) = rx.recv() => {
                    //Make sure to send same type of data to a single sender so we can get the type
                    let table = data.first().expect("Unreachable logic reached. Check quick_stream::upsert::process_n<T>(&self, multi_table_single_queries: MultiTableSingleQueryHolder, rx: Receiver<Vec<T>>, thread_id: i64, n: usize) function").table();
                    trace!("{}:{}:{}: data received pushing for ingestion to table: {}. pkeys: {:?}", self.name, n, thread_id, table, data.iter().map(|f| f.pkey()).collect::<Vec<i64>>());
                    let count = T::upsert(&client, data, &statement_map.get(&table).unwrap(), thread_id).await?;
                    trace!("{}:{}:{}: data ingestion to table: {} successfull. count: {}", self.name, n, thread_id, table, count);
                }
                _ = self.cancellation_token.cancelled() => {
                    info!("{}:{}:{}: cancellation token received. shutting down data ingestor", self.name, n, thread_id);
                    break 'inner
                }
            }
        }

        info!("{}:{}:{}: closing the channel", self.name, n, thread_id);
        drop(rx);
        

        info!("{}:{}:{} shutting down data ingestor", self.name, n, thread_id);
        Ok(())
    }

    /**
     * n is redunt here as n is the same as type_ ***need to remove n***
     */
    fn init_sender<T>(&self, n: usize, count: usize, tx_count: &mut i64, type_: usize) -> Vec<UpsertData<T>> where T: MultiTableUpsert<T> + Clone + Send + 'static {
        trace!("{}: initiating sender, creating {} upsert senders", self.name, count);
        let mut senders = vec![];
    
        for _ in 0..count {
            let (tx_t, rx_t) = mpsc::channel::<Vec<T>>(self.buffer_size);
    
            let thread_id = tx_count.clone();
            let query = self.queries.get(&n);
            let n_clone = n.clone();
            let self_clone = self.to_owned();
            let handler = tokio::spawn(async move {
                let _ = self_clone.process_n(query, rx_t, thread_id, n_clone).await;
                1u8
            });
    
            let tx_struct = UpsertData::new(tx_t, handler, tx_count.clone(), type_);
    
            *tx_count += 1;
    
            senders.push(tx_struct);
        }
    
        senders
    }

    fn init_senders<T>(&self, tx_count: &mut i64) -> HashMap<usize, Vec<UpsertData<T>>> where T: MultiTableUpsert<T> + Clone + Send + 'static {
        trace!("{}: creating sender map of capacity 11", self.name);
        let mut sender_map = HashMap::with_capacity(11);
        
        trace!("{}: creating data senders from 1-10 and 100", self.name);
        let senders_1 = self.init_sender::<T>(1, self.single_digits, tx_count, 1);
        let senders_2 = self.init_sender::<T>(2, self.single_digits, tx_count, 2);
        let senders_3 = self.init_sender::<T>(3, self.single_digits, tx_count, 3);
        let senders_4 = self.init_sender::<T>(4, self.single_digits, tx_count, 4);
        let senders_5 = self.init_sender::<T>(5, self.single_digits, tx_count, 5);
        let senders_6 = self.init_sender::<T>(6, self.single_digits, tx_count, 6);
        let senders_7 = self.init_sender::<T>(7, self.single_digits, tx_count, 7);
        let senders_8 = self.init_sender::<T>(8, self.single_digits, tx_count, 8);
        let senders_9 = self.init_sender::<T>(9, self.single_digits, tx_count, 9);
        let senders_10 = self.init_sender::<T>(10, self.tens, tx_count, 10);
        trace!("{}: creating data senders from 1-10 success", self.name);

        let senders_100 = self.init_sender::<T>(1, self.hundreds, tx_count, 100);
        trace!("{}: creating data senders for 100 success", self.name);

        sender_map.insert(1, senders_1);
        sender_map.insert(2, senders_2);
        sender_map.insert(3, senders_3);
        sender_map.insert(4, senders_4);
        sender_map.insert(5, senders_5);
        sender_map.insert(6, senders_6);
        sender_map.insert(7, senders_7);
        sender_map.insert(8, senders_8);
        sender_map.insert(9, senders_9);
        sender_map.insert(10, senders_10);

        sender_map.insert(100, senders_100);

        self.print_sender_status(&sender_map, &tx_count);

        sender_map
    }

    async fn push_to_handle<T>(&self, senders: &mut HashMap<usize, Vec<UpsertData<T>>>, vec_data: Vec<Vec<T>>, tx_count: &mut i64) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        for data in vec_data {
            let k = data.len();
            self.handle_n(data,
                 senders.get_mut(&k)
                    .expect("Unreachable logic reached. Check quick_stream::split_vec<T>(data: Vec<T>) function"), 
                 tx_count, k).await;
        }
    }

    async fn handle_n<T>(&self, data: Vec<T>, senders: &mut Vec<UpsertData<T>>, tx_count: &mut i64, type_: usize) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        trace!("{}: handeling data started", self.name);
        trace!("{}: sorting senders by capacity to get the channel with highest capacity", self.name);
        senders.sort_by(|x, y| y.tx.capacity().cmp(&x.tx.capacity()));

        let sender_0 = match senders.first() {
            Some(sender) => sender,
            None => {
                error!("{}: no senders found, this is an impossible scenario", self.name);
                panic!("no senders found, impossible scenario")
            },
        };

        let capacity = sender_0.tx.capacity() as f64 / self.buffer_size as f64 * 100f64;

        if capacity <= self.connection_creation_threshold {
            warn!("{}: capacity of {}:{} {}% is below connection creation threshold {}%", self.name, sender_0.type_, sender_0.id, capacity, self.connection_creation_threshold);

            if *tx_count < self.max_con_count as i64 {
                info!("{}: creating a sender of type {} since current connections {} is below allowed max connections count {}", self.name, type_, *tx_count, self.max_con_count);
                let (tx_t, rx_t) = mpsc::channel::<Vec<T>>(self.buffer_size);

                let thread_id = tx_count.clone();
                let n = data.len();
                let query = self.queries.get(&n);
                let self_clone = Arc::new(self.to_owned());
                let handler = tokio::spawn(async move {
                    let _ = self_clone.process_n(query, rx_t, thread_id, n).await;
                    0u8
                });

                match tx_t.send(data).await {
                    Ok(_) => {
                        let tx_struct = UpsertData::new(tx_t, handler, tx_count.clone(), type_);
                        info!("{}: creating sender {}:{} successful", self.name, tx_struct.type_, tx_struct.id);
                        *tx_count += 1;
                        senders.push(tx_struct);

                        if *tx_count == self.max_con_count as i64 {
                            warn!("{}: max connection count reached", self.name)
                        } else {
                            info!("{}: connection created, current total connections : {}", self.name, tx_count)
                        }
                    },
                    Err(error) => {
                        error!("{}: creating sender failed with error: {}", self.name, error);
                        panic!("{}: failed to send data through the newly created channel {}", self.name, error)
                    },
                };
            } else {
                error!("{}: unable to create connection as max connection count has already reached", self.name);
                warn!("{}: PROCESSOR WILL HAVE TO WAIT UNTIL CAPACITY IS AVAIALABLE TO PROCEED", self.name);
                match sender_0.tx.send(data).await {
                    Ok(_) => info!("{}: data successfully pushed after capacity was available", self.name),
                    Err(error) => {
                        panic!("{}: failed to send data through the channel of sender {}:{} : {}", self.name, sender_0.type_, sender_0.id, error)
                    },
                }
            }
        } else {
            info!("{}: capacity of sender {}:{} is at {}%", self.name, sender_0.type_, sender_0.id, capacity);
            match sender_0.tx.send(data).await {
                Ok(_) => {
                    trace!("{}: pushing to data ingestor success using sender {}:{}", self.name, sender_0.type_, sender_0.id);
                },
                Err(error) => {
                    panic!("{}: failed to send data through the channel of sender {}:{} : {}", self.name, sender_0.type_, sender_0.id, error)
                },
            };
        }
    }

    fn re_balance_sender<T>(&self, senders: &mut Vec<UpsertData<T>>, init_limit: usize, tx_count: &mut i64, type_: usize) -> bool where T: MultiTableUpsert<T> + Clone + Send + 'static {

        trace!("{}: rebalancing senders of type {}", self.name, type_);

        let start_senders = senders.len();
        senders.retain(|upsert_data| !upsert_data.tx.is_closed() || upsert_data.join_handler.is_finished());

        let removed_senders = start_senders - senders.len();

        if removed_senders > 0 {
            info!("{}: removed {} senders of type {}", self.name, removed_senders, type_);
            *tx_count -= removed_senders as i64;
        }

        if senders.len() > init_limit {
            let full_capacity_count = senders.iter().filter(|sender| sender.tx.capacity() == self.buffer_size).collect::<Vec<&UpsertData<T>>>().len();
    
            if full_capacity_count > 0 {
                let mut amount_to_pop = full_capacity_count - (full_capacity_count / 2usize);
                if senders.len() - amount_to_pop < init_limit {
                    amount_to_pop = senders.len() - init_limit;
                }
                senders.sort_by(|x, y| x.tx.capacity().cmp(&y.tx.capacity()));
                for _ in 0..amount_to_pop {
                    senders.pop();
                    *tx_count -= 1;
                }
            }
        }

        trace!("{}: rebalancing senders of type {} complete", self.name, type_);
        senders.len() != start_senders
    }

    fn rebalance_senders<T>(&self, senders: &mut HashMap<usize, Vec<UpsertData<T>>>, tx_count: &mut i64) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        trace!("{}: rebalancing database connections", self.name);
        let mut rebalanced = false;
        senders.iter_mut().for_each(|(sender_type, sender)| {
            if *sender_type < 10 {
                if self.re_balance_sender(sender, self.single_digits, tx_count, *sender_type) {
                    rebalanced = true
                }
            } else if *sender_type == 10 {
                if self.re_balance_sender(sender, self.tens, tx_count, *sender_type) {
                    rebalanced = true
                }
            } else if *sender_type == 100 {
                if self.re_balance_sender(sender, self.hundreds, tx_count, *sender_type) {
                    rebalanced = true
                }
            } else {
                error!("{}: Impossible Scenario, Check quick_stream::upsert::init_senders<T>(&self, tx_count: &mut i64) function", self.name);
                panic!("Unreachable logic reached. Check quick_stream::upsert::init_senders<T>(&self, tx_count: &mut i64) function")
            }
        });

        if rebalanced || self.print_con_config {
            self.print_sender_status(&senders, &tx_count)
        }
    }

    fn print_sender_status<T>(&self, senders: &HashMap<usize, Vec<UpsertData<T>>>, tx_count: &i64) where T: MultiTableUpsert<T> + Clone + Send + 'static {
        let total_senders_percentage = (*tx_count * 100) as f64 / self.max_con_count as f64;
        info!(" {}: Current Senders (Database Connections) configuration
                SENDER          AMOUNT
            senders     1   :     {}
            senders     2   :     {}
            senders     3   :     {}
            senders     4   :     {}
            senders     5   :     {}
            senders     6   :     {}
            senders     7   :     {}
            senders     8   :     {}
            senders     9   :     {}
            senders    10   :     {}
            senders   100   :     {}
            ____________________________
            total senders   :     {}
            total senders % :     {}
            ============================
        ", 
        self.name, 
        senders.get(&1).unwrap().len(), 
        senders.get(&2).unwrap().len(), 
        senders.get(&3).unwrap().len(), 
        senders.get(&4).unwrap().len(), 
        senders.get(&5).unwrap().len(), 
        senders.get(&6).unwrap().len(), 
        senders.get(&7).unwrap().len(), 
        senders.get(&8).unwrap().len(), 
        senders.get(&9).unwrap().len(), 
        senders.get(&10).unwrap().len(), 
        senders.get(&100).unwrap().len(),
        *tx_count,
        total_senders_percentage)
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod test{
    use std::{collections::HashMap, time::Duration};

    use async_trait::async_trait;
    use chrono::NaiveDateTime;
    use tokio::{sync::mpsc, time};
    use tokio_postgres::{types::ToSql, Client, Error, Statement};
    use tokio_util::sync::CancellationToken;

    use crate::{builder::{support::{MultiTableUpsertQueryHolder, QueryHolder}, QuickStreamBuilder}, upsert::{multi_table_upsert::MultiTableUpsert, Upsert}};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Test1 {
        id: i64,
        comment: String
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Test2 {
        id: i64,
        comment: String
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Test {
        id: i64,
        modified_date: NaiveDateTime,
        table: String,
        test1: Option<Test1>,
        test2: Option<Test2>
    }

    impl MultiTableUpsert<Self> for Test {
        fn table(&self) -> String {
            self.table.clone()
        }

        fn tables() -> Vec<String> {
            vec![String::from("test1"), String::from("test2")]
        }
    }

    #[async_trait]
    impl Upsert<Test> for Test {
        async fn upsert(
            client: &Client,
            data: Vec<Test>,
            statement: &Statement,
            thread_id: i64,
        ) -> Result<u64, Error> {
            println!("data received, data: {:#?}, {}", data, thread_id);
            let mut params: Vec<&(dyn ToSql + Sync)> = vec![];

            if data.first().unwrap().table == "test1" {
                for d in data.iter() {
                    params.push(&d.id);
                    params.push(&d.test1.as_ref().unwrap().comment);
                }
            } else {
                for d in data.iter() {
                    params.push(&d.id);
                    params.push(&d.test2.as_ref().unwrap().comment);
                }
            }
        
            client.execute(statement, &params).await.unwrap();
            Ok(1)
        }

        fn modified_date(&self) -> NaiveDateTime {
            self.modified_date
        }

        fn pkey(&self) -> i64 {
            self.id
        }
    }

    #[ignore = "only works with a database connection"]
    #[tokio::test]
    async fn test_db() {
        let mut quick_stream_builder = QuickStreamBuilder::default();

        let cancellation_token = CancellationToken::new();
        let mut db_config = tokio_postgres::Config::new();
        db_config.host("127.0.0.1")
        .user("unit_test")
        .password("production")
        .dbname("unit_test_db")
        .port(5432);

        let mut test1_queries = QueryHolder::default();
        test1_queries.set_n(1, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        //Adding just for testing purposes
        test1_queries.set_n(2, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(3, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(4, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(5, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(6, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(7, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(8, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(9, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(10, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(100, String::from("insert into test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        let mut test2_queries = QueryHolder::default();

        test2_queries.set_n(1, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        //Adding just for testing purposes
        test2_queries.set_n(2, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(3, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(4, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(5, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(6, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(7, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(8, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(9, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(10, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(100, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));


        let mut query_holders = HashMap::with_capacity(2);
        query_holders.insert(String::from("test1"), test1_queries);
        query_holders.insert(String::from("test2"), test2_queries);

        let multi_table_query_holder = MultiTableUpsertQueryHolder::new(query_holders);

        quick_stream_builder.cancellation_tocken(cancellation_token)
        .max_connection_count(5)
        .buffer_size(10)
        .single_digits(2)
        .tens(2)
        .hundreds(1)
        .db_config(db_config)
        .multi_table_queries(multi_table_query_holder)
        .max_records_per_cycle_batch(10)
        .introduced_lag_cycles(2)
        .introduced_lag_in_millies(100)
        .connection_creation_threshold(50.0)
        .print_connection_configuration();

        let multi_table_upsert_quick_stream = quick_stream_builder.build_multi_part_upsert();

        let db_client = multi_table_upsert_quick_stream.get_db_client().await;

        let _ = db_client.prepare("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2").await.unwrap();
        
    }

    #[ignore = "only works with a database connection"]
    #[tokio::test]
    async fn test_functionality() {
        let mut quick_stream_builder = QuickStreamBuilder::default();

        let cancellation_token = CancellationToken::new();
        let mut db_config = tokio_postgres::Config::new();
        db_config.host("127.0.0.1")
        .user("unit_test")
        .password("production")
        .dbname("unit_test_db")
        .port(5432);

        let mut test1_queries = QueryHolder::default();
        test1_queries.set_n(1, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        //Adding just for testing purposes
        test1_queries.set_n(2, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(3, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(4, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(5, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(6, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(7, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(8, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(9, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(10, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test1_queries.set_n(100, String::from("insert into quick_stream.test1 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        let mut test2_queries = QueryHolder::default();

        test2_queries.set_n(1, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));

        //Adding just for testing purposes
        test2_queries.set_n(2, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(3, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(4, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(5, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(6, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(7, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(8, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(9, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(10, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));
        test2_queries.set_n(100, String::from("insert into quick_stream.test2 (id, comment) values ($1, $2) on conflict (id) do update set comment = $2"));


        let mut query_holders = HashMap::with_capacity(2);
        query_holders.insert(String::from("test1"), test1_queries);
        query_holders.insert(String::from("test2"), test2_queries);

        let multi_table_query_holder = MultiTableUpsertQueryHolder::new(query_holders);

        quick_stream_builder.cancellation_tocken(cancellation_token)
        .max_connection_count(5)
        .buffer_size(10)
        .single_digits(2)
        .tens(2)
        .hundreds(1)
        .db_config(db_config)
        .multi_table_queries(multi_table_query_holder)
        .max_records_per_cycle_batch(10)
        .introduced_lag_cycles(2)
        .introduced_lag_in_millies(100)
        .connection_creation_threshold(50.0)
        .print_connection_configuration();

        let multi_table_upsert_quick_stream = quick_stream_builder.build_multi_part_upsert();

        let (tx, rx) = mpsc::channel::<Vec<Test>>(10);

        let _ = tokio::spawn(async move {
            multi_table_upsert_quick_stream.run(rx).await;
            66u8
        });

        let test1 = Test1 {
            id: 3,
            comment: String::from("Test Data 1 (re)")
        };

        let test2 = Test2 {
            id: 5,
            comment: String::from("Test Data 2 (re)")
        };

        let test_1 = Test {
            id: test1.id,
            modified_date: chrono::Utc::now().naive_utc(),
            table: String::from("test1"),
            test1: Some(test1),
            test2: None
        };

        let test_2 = Test {
            id: test2.id,
            modified_date: chrono::Utc::now().naive_utc(),
            table: String::from("test2"),
            test1: None,
            test2: Some(test2)
        };

        let data = vec![test_1, test_2];

        tx.send(data).await.unwrap();

        time::sleep(Duration::from_secs(10)).await;
    }
}