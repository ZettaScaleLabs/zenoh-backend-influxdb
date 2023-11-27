//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use async_std::task;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as b64_std_engine, Engine};
use influxdb;//::{Client, ReadQuery as InfluxRQuery, Timestamp as InfluxTimestamp, WriteQuery as InfluxWQuery};

use futures::prelude::*;
use influxdb2::api::buckets::ListBucketsRequest;
use influxdb2::models::{DataPoint, PostBucketRequest};
use influxdb2::Client;


use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use influxdb2::{ FromDataPoint};
use influxdb2::models::Query;
use chrono::Utc;
use futures::prelude::*;

//use influxdb2::Client;
use influxdb2_derive::WriteDataPoint;
//#[derive(Debug)]





use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};
//use std::intrinsics::const_eval_select;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use zenoh::buffers::{SplitBuffer, ZBuf};
use zenoh::prelude::*;
use zenoh::properties::Properties;
use zenoh::selector::TimeExpr;
use zenoh::time::Timestamp;
use zenoh::Result as ZResult;
use zenoh_backend_traits::config::{
    PrivacyGetResult, PrivacyTransparentGet, StorageConfig, VolumeConfig,
};
use zenoh_backend_traits::StorageInsertionResult;
use zenoh_backend_traits::*;
use zenoh_core::{bail, zerror};
use zenoh_util::{Timed, TimedEvent, TimedHandle, Timer};

// Properties used by the Backend
pub const PROP_BACKEND_URL: &str = "url";
pub const PROP_BACKEND_ORG_ID: &str = "orgID";

pub const PROP_BACKEND_INFLUXDB_VERSION: &str = "influxdb_version";
pub const PROP_TOKEN: &str = "token";

// Properties used by the Storage
pub const PROP_STORAGE_DB: &str = "db";
pub const PROP_STORAGE_CREATE_DB: &str = "create_db";
pub const PROP_STORAGE_ON_CLOSURE: &str = "on_closure";

// Special key for None (when the prefix being stripped exactly matches the key)
pub const NONE_KEY: &str = "@@none_key@@";

// delay after deletion to drop a measurement
const DROP_MEASUREMENT_TIMEOUT_MS: u64 = 5000;

const GIT_VERSION: &str = git_version::git_version!(prefix = "v", cargo_prefix = "v");
lazy_static::lazy_static!(
    static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
    static ref INFLUX_REGEX_ALL: String = key_exprs_to_influx_regex(&["**".try_into().unwrap()]);
);

#[allow(dead_code)]
const CREATE_BACKEND_TYPECHECK: CreateVolume = create_volume;

type Config<'a> = &'a serde_json::Map<String, serde_json::Value>;

fn get_private_conf<'a>(config: Config<'a>, credit: &str) -> ZResult<Option<&'a String>> {
    match config.get_private(credit) {
        PrivacyGetResult::NotFound => Ok(None),
        PrivacyGetResult::Private(serde_json::Value::String(v)) => Ok(Some(v)),
        PrivacyGetResult::Public(serde_json::Value::String(v)) => {
            log::warn!(
                r#"Value "{}" is given for `{}` publicly (i.e. is visible by anyone who can fetch the router configuration). You may want to replace `{}: "{}"` with `private: {{{}: "{}"}}`"#,
                v,
                credit,
                credit,
                v,
                credit,
                v
            );
            Ok(Some(v))
        }
        PrivacyGetResult::Both {
            public: serde_json::Value::String(public),
            private: serde_json::Value::String(private),
        } => {
            log::warn!(
                r#"Value "{}" is given for `{}` publicly, but a private value also exists. 
                The private value will be used, but the public value, which is {} the same as the private one, will still be visible in configurations."#,
                public,
                credit,
                if public == private { "" } else { "not " }
            );
            Ok(Some(private))
        }
        _ => {
            bail!("Optional property `{}` must be a string", credit)
        }
    }
}

// Keep in mind that Influx v1 allows access without credentials
fn extract_credentials(config: Config) -> ZResult<Option<InfluxDbCredentials>> {
    if let Ok(Some(token)) = get_private_conf(&config, PROP_TOKEN) {
        return Ok(Some(InfluxDbCredentials::Token(token.clone())));
    } else {
        return Ok(None);
    }
}


fn get_url(config :VolumeConfig)->ZResult<String>{
    let url = match config.rest.get(PROP_BACKEND_URL) {
        Some(serde_json::Value::String(url)) => return Ok(url.clone()),
        _ => {
            bail!(
                "Mandatory property `{}` for InfluxDb Backend must be a string",
                PROP_BACKEND_URL
            )
        }
    };
}

fn get_org_id(config :VolumeConfig)->ZResult<String>{
    let org_id = match config.rest.get(PROP_BACKEND_ORG_ID) {
        Some(serde_json::Value::String(url)) => return Ok(url.clone()),
        _ => {
            bail!(
                "Mandatory property `{}` for InfluxDb Backend must be a string",
                PROP_BACKEND_ORG_ID
            )
        }
    };
}


#[no_mangle]
pub fn create_volume(mut config: VolumeConfig) -> ZResult<Box<dyn Volume>> {
    // For some reasons env_logger is sometime not active in a loaded library.
    // Try to activate it here, ignoring failures.
    let _ = env_logger::try_init();
    debug!("InfluxDB backend {}", LONG_VERSION.as_str());

    config
        .rest
        .insert("version".into(), LONG_VERSION.clone().into());
    let url = get_url(config).expect("url not found");
    let org_id = get_org_id(config).expect("org_id not found");

    // The InfluxDB client used for creating and dropping buckets (create/drop databases)
    let mut admin_client: Client;
    let credentials = extract_credentials(&config.rest)?;
    match &credentials {
        Some(InfluxDbCredentials::Token(token)) => {
            //need org name and admin token
            admin_client = Client::new(url, org_id, token); //what if it panics (the library unwraps this)
        }
        _ => {}
    }

    // Check connectivity to InfluxDB
    match async_std::task::block_on(async { admin_client.ready().await }) {
        Ok(res) => {
            if res == false {
                bail!("Influxdb server is not ready! ")
            }
        }
        Err(e) => bail!("Failed to create InfluxDb Volume : {:?}", e),
    }

    //if everything works, the volume is created here
    Ok(Box::new(InfluxDbBackend {
        admin_status: config,
        admin_client,
        credentials,
    }))
}

// Enum to take care of different credential forms
// Keep in mind that influx v1 allow access without credentials
#[derive(Debug)]
enum InfluxDbCredentials {
    Token(String),
}

//do we need influxdb_version or can we find that from the token/usr-psswd situation
pub enum InfluxDbVersion {
    VersionOne,
    VersionTwo,
}

pub struct InfluxDbBackend {
    admin_status: VolumeConfig,
    admin_client: Client,
    credentials: Option<InfluxDbCredentials>,
}

#[async_trait]
impl Volume for InfluxDbBackend {
    fn get_admin_status(&self) -> serde_json::Value {
        self.admin_status.to_json_value()
    }

    fn get_capability(&self) -> Capability {
        Capability {
            persistence: Persistence::Durable,
            history: History::All,
            read_cost: 1,
        }
    }

    async fn create_storage(&mut self, mut config: StorageConfig) -> ZResult<Box<dyn Storage>> {
        let volume_cfg = match config.volume_cfg.as_object() {
            Some(v) => v,
            None => bail!("influxdb backed storages need some volume-specific configuration"),
        };

        let on_closure = match volume_cfg.get(PROP_STORAGE_ON_CLOSURE) {
            Some(serde_json::Value::String(x)) if x == "drop_series" => OnClosure::DropSeries,
            Some(serde_json::Value::String(x)) if x == "drop_db" => OnClosure::DropDb,
            Some(serde_json::Value::String(x)) if x == "do_nothing" => OnClosure::DoNothing,
            None => OnClosure::DoNothing,
            Some(_) => {
                bail!(
                    r#"`{}` property of storage `{}` must be one of "do_nothing" (default), "drop_db" and "drop_series""#,
                    PROP_STORAGE_ON_CLOSURE,
                    &config.name
                )
            }
        };
        let (db, createdb) = match volume_cfg.get(PROP_STORAGE_DB) {
            Some(serde_json::Value::String(s)) => (
                s.clone(),
                match volume_cfg.get(PROP_STORAGE_CREATE_DB) {
                    None | Some(serde_json::Value::Bool(false)) => false,
                    Some(serde_json::Value::Bool(true)) => true,
                    Some(_) => todo!(),
                },
            ),
            None => (generate_db_name(), true),
            _ => bail!(""),
        };

        // The Influx client on database used to write/query on this storage
        // (using the same URL than backend's admin_client, but with storage credentials)
        let mut client: Client; // = Client::new(self.admin_status.get(PROP_BACKEND_URL), &db);
        let url = get_url(self.admin_status).expect("url not found");
        let org_id = get_org_id(self.admin_status).expect("org_id not found");

        // Use token creds if specified in storage's volume config
        // create user with token creds if user exists, if no token, then abort
        match extract_credentials(&volume_cfg)? {
            Some(InfluxDbCredentials::Token(token)) => {
                client = Client::new(url, org_id, &db);
                //None
            }
            _ => bail!("No credentials specified to access database '{}'", db),
        };

        // Check if the database exists (using storages credentials)
       // let db_exists = ;
        //let rw_allowed = is_rw_allowed(&client, &db).await?;
        if !is_db_existing(&client, &db).await? {
            if createdb {
                // try to create db using user credentials
                if !create_db(&client, &org_id, &db).await.unwrap() {
                    if !create_db(&self.admin_client, &org_id, &db).await.unwrap() {
                        bail!("Database '{}' wasnt't created in InfluxDb", db)
                    }
                }
                //database/bucket is created now
            }
        }

        // re-insert the actual name of database (in case it has been generated)
        config
            .volume_cfg
            .as_object_mut()
            .unwrap()
            .entry(PROP_STORAGE_DB)
            .or_insert(db.clone().into());

        //insert url and org_id in config
        config
            .volume_cfg
            .as_object_mut()
            .unwrap()
            .entry(PROP_BACKEND_URL)
            .or_insert(url.clone().into());

        config
            .volume_cfg
            .as_object_mut()
            .unwrap()
            .entry(PROP_BACKEND_ORG_ID)
            .or_insert(org_id.clone().into());

        // The Influx client on database with backend's credentials (admin), to drop measurements and database
        //why not clone admin client here??
        let mut admin_client: Client;
        match &self.credentials {
            Some(InfluxDbCredentials::Token(token)) => {
                admin_client = Client::new(url, org_id, token);
            }
            _ => {}
        }

        Ok(Box::new(InfluxDbStorage {
            config,
            admin_client,
            client,
            on_closure,
            timer: Timer::default(),
        }))
    }

    fn incoming_data_interceptor(&self) -> Option<Arc<dyn Fn(Sample) -> Sample + Send + Sync>> {
        None
    }

    fn outgoing_data_interceptor(&self) -> Option<Arc<dyn Fn(Sample) -> Sample + Send + Sync>> {
        None
    }
}

enum OnClosure {
    DropDb,
    DropSeries,
    DoNothing,
}

impl TryFrom<&Properties> for OnClosure {
    type Error = zenoh_core::Error;
    fn try_from(p: &Properties) -> ZResult<OnClosure> {
        match p.get(PROP_STORAGE_ON_CLOSURE) {
            Some(s) => {
                if s == "drop_db" {
                    Ok(OnClosure::DropDb)
                } else if s == "drop_series" {
                    Ok(OnClosure::DropSeries)
                } else {
                    bail!("Unsupported value for 'on_closure' property: {}", s)
                }
            }
            None => Ok(OnClosure::DoNothing),
        }
    }
}

struct InfluxDbStorage {
    config: StorageConfig,
    admin_client: Client,
    client: Client,
    on_closure: OnClosure,
    timer: Timer,
}

impl InfluxDbStorage {
    /* 
    async fn get_deletion_timestamp(&self, measurement: &str) -> ZResult<Option<Timestamp>> {
        #[derive(Deserialize, Debug, PartialEq)]
        struct QueryResult {
            timestamp: String,
        }

        let query = InfluxRQuery::new(format!(
            r#"SELECT "timestamp" FROM "{measurement}" WHERE kind='DEL' ORDER BY time DESC LIMIT 1"#
        ));
        match self.client.json_query(query).await {
            Ok(mut result) => match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() && !qr.series[0].values.is_empty() {
                        let ts = qr.series[0].values[0]
                            .timestamp
                            .parse::<Timestamp>()
                            .map_err(|err| {
                                zerror!(
                                "Failed to parse the latest timestamp for deletion of measurement {} : {}",
                                measurement, err.cause)
                            })?;
                        Ok(Some(ts))
                    } else {
                        Ok(None)
                    }
                }
                Err(err) => bail!(
                    "Failed to get latest timestamp for deletion of measurement {} : {}",
                    measurement,
                    err
                ),
            },
            Err(err) => bail!(
                "Failed to get latest timestamp for deletion of measurement {} : {}",
                measurement,
                err
            ),
        }
    }
*/
    async fn schedule_measurement_drop(&self, measurement: &str) -> TimedHandle {
        let event = TimedEvent::once(
            Instant::now() + Duration::from_millis(DROP_MEASUREMENT_TIMEOUT_MS),
            TimedMeasurementDrop {
                client: self.admin_client.clone(),
                measurement: measurement.to_string(),
            },
        );
        let handle = event.get_handle();
        self.timer.add_async(event).await;
        handle
    }

    fn keyexpr_from_serie(&self, serie_name: &str) -> ZResult<Option<OwnedKeyExpr>> {
        if serie_name.eq(NONE_KEY) {
            Ok(None)
        } else {
            match OwnedKeyExpr::from_str(serie_name) {
                Ok(key) => Ok(Some(key)),
                Err(e) => Err(format!("{}", e).into()),
            }
        }
    }
}

#[async_trait]
impl Storage for InfluxDbStorage {
    fn get_admin_status(&self) -> serde_json::Value {
        // TODO: possibly add more properties in returned Value for more information about this storage
        self.config.to_json_value()
    }

    async fn put(
        &mut self,
        key: Option<OwnedKeyExpr>,
        value: Value,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let measurement = key.unwrap_or_else(|| OwnedKeyExpr::from_str(NONE_KEY).unwrap());

        // Note: assume that uhlc timestamp was generated by a clock using UNIX_EPOCH (that's the case by default)
        let influx_time = timestamp.get_time().to_duration().as_nanos();

        // get timestamp of deletion of this measurement, if any
        /*
        if let Some(del_time) = self.get_deletion_timestamp(measurement.as_str()).await? {
            // ignore sample if oldest than the deletion
            if timestamp < del_time {
                debug!(
                    "Received a value for {:?} with timestamp older than its deletion; ignore it",
                    measurement
                );
                return Ok(StorageInsertionResult::Outdated);
            }
        }
        */

        // encode the value as a string to be stored in InfluxDB, converting to base64 if the buffer is not a UTF-8 string
        let (base64, strvalue) = match String::from_utf8(value.payload.contiguous().into_owned()) {
            Ok(s) => (false, s),
            Err(err) => (true, b64_std_engine.encode(err.into_bytes())),
        };

       let db =  match self.config.volume_cfg.get(PROP_STORAGE_DB){
        Some(db)=> db.to_string(),
        _=> bail!("no db was found"),
       };
        // Note: tags are stored as strings in InfluxDB, while fileds are typed.
        // For simpler/faster deserialization, we store encoding, timestamp and base64 as fields.
        // while the kind is stored as a tag to be indexed by InfluxDB and have faster queries on it.
 
        let ZenohPoint = vec![
            DataPoint::builder(measurement)
                .tag("kind", "PUT")
                .tag("ztimestamp", timestamp.to_string())
                .field("encoding_prefix", u8::from(*value.encoding.prefix()).to_string())   //issue with this data-type, converting it to string for now, will see later
                .field("encoding_suffix", value.encoding.suffix())
                .field("base64", base64)
                .field("value", strvalue)
                .build()
                .expect("error in building the write query"),
        ];
        if let Err(e) = self.client.write(&db, stream::iter(ZenohPoint)).await {
            error!(
                "Failed to write. Does not have write permissions for the db: {:?}",
                e
            );
            bail!(
                "Failed to put Value for {:?} in InfluxDb storage : {}",
                measurement,
                e
            )
        } else {
            Ok(StorageInsertionResult::Inserted)
        }
    }

    async fn delete(
        &mut self,
        key: Option<OwnedKeyExpr>,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let measurement = key.unwrap_or_else(|| OwnedKeyExpr::from_str(NONE_KEY).unwrap());

        // Note: assume that uhlc timestamp was generated by a clock using UNIX_EPOCH (that's the case by default)
        // delete all points from the measurement that are older than this DELETE message
        // (in case more recent PUT have been recevived un-ordered)
        // let query = InfluxRQuery::new(format!(
        //     r#"DELETE FROM "{}" WHERE time < {}"#,
        //     measurement, influx_time
        // ));

        let db =  match self.config.volume_cfg.get(PROP_STORAGE_DB){
            Some(db)=> db.to_string(),
            _=> bail!("no db was specified"),
           };
        let start = NaiveDateTime::from_timestamp_millis(0).unwrap();
        let deletion_time = NaiveDateTime::from_timestamp_millis(timestamp.get_time().0.try_into().unwrap()).unwrap();
       // let del_time = timestamp.get_time();
        let predicate = None;   //can be specified as tag or field values
        debug!("Delete {:?} with Influx query", measurement);
        if let Err(e) = self.client.delete(&db,start, deletion_time, predicate).await {
            bail!(
                "Failed to delete points for measurement '{}' from InfluxDb storage : {}",
                measurement,
                e
            )
        }
        // store a point (with timestamp) with "delete" tag, thus we don't re-introduce an older point later
        let points = vec![
            DataPoint::builder(measurement)
                .tag("kind", "DELETE")
                .tag("ztimestamp", timestamp.to_string())
                .build()
                .expect("error in building the mark delete query"),
        ];
        
        debug!(
            "Mark measurement {} as deleted at time {}",
            measurement, deletion_time
        );
        if let Err(e) = self.client.write(&db, stream::iter(points)).await {
            bail!(
                "Failed to mark measurement {:?} as deleted : {}",
                measurement,
                e
            )
        }
        // schedule the drop of measurement later in the future, if it's empty
        let _ = self.schedule_measurement_drop(measurement.as_str()).await;
        Ok(StorageInsertionResult::Deleted)
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let measurement = match key.clone() {
            Some(k) => k,
            None => OwnedKeyExpr::from_str(NONE_KEY).unwrap(),
        };
        // convert the key expression into an Influx regex
        let regex = key_exprs_to_influx_regex(&[&KeyExpr::from(measurement)]);

        // construct the Influx query clauses from the parameters
        let clauses = clauses_from_parameters(parameters)?;

        // the Influx query
        let influx_query_str = format!("SELECT * FROM {regex} {clauses}");

        let db =  match self.config.volume_cfg.get(PROP_STORAGE_DB){
            Some(db)=> db.to_string(),
            _=> bail!("no db was found"),
           };
        //let influx_query = InfluxRQuery::new(&influx_query_str);

        // the expected JSon type resulting from the query
       // #[derive(Deserialize, Debug)]
        #[derive(Debug, Default, FromDataPoint)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            ztimestamp: String,
            encoding_prefix: String,    //is u8 but working around that for now
            encoding_suffix: String,
            base64: bool,
            value: String,
        }

        debug!("Get {:?} with Influx query: {}", key, influx_query_str);
        let data_range ="-1w";
        let qs = format!("from(bucket: \"{}\")
                                    |> range(start: {})
                                    |> filter(fn: (r) => r._measurement == \"{}\")
                                    |> last()
                                ",
                                db,data_range,measurement);
     let query = Query::new(qs.to_string());
     let res: Vec<ZenohPoint> = self.client.query::<ZenohPoint>(Some(query))
         .await?;
     println!("{:?}", res);
     let result :Vec<StoredData>;
    for i in &res{
        //let value = Value::new(payload).encoding(encoding);
        let ztimestamp = Timestamp::from_str(&i.ztimestamp).expect("didnt parse timestamp");
        result.push(StoredData { value:i.value.into(), timestamp:ztimestamp });
    }
     Ok(result)

    }


    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>>{

        let db =  match self.config.volume_cfg.get(PROP_STORAGE_DB){
            Some(db)=> db.to_string(),
            _=> bail!("no db was found"),
           };
        //let influx_query = InfluxRQuery::new(&influx_query_str);

        // the expected JSon type resulting from the query
       // #[derive(Deserialize, Debug)]
        #[derive(Debug, Default, FromDataPoint)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            ztimestamp: String,
            encoding_prefix: String,    //is u8 but working around that for now
            encoding_suffix: String,
            base64: bool,
            value: String,
        }

       // debug!("Get {:?} with Influx query: {}", key, influx_query_str);
        let data_range ="-1w";
        let qs = format!("from(bucket: \"{}\")
                                    |> range(start: {})
                                ",
                                db,data_range);
     let query = Query::new(qs.to_string());
     let res: Vec<ZenohPoint> = self.client.query::<ZenohPoint>(Some(query))
         .await?;
     println!("{:?}", res);
     let result :Vec<StoredData>;
    for i in &res{
        //let value = Value::new(payload).encoding(encoding);
        let ztimestamp = Timestamp::from_str(&i.ztimestamp).expect("didnt parse timestamp");
        result.push(StoredData { value:Some(i.value.into()), timestamp:ztimestamp });
    }
     Ok(result)
        //self.get();
    }
    /*
    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>> {
        let mut result = Vec::new();

        // the Influx query: 1 entry == 1 measurement => get only 1 point per measurement (the more recent timestamp)
        let influx_query_str = format!(
            "SELECT * FROM {} ORDER BY time DESC LIMIT 1",
            *INFLUX_REGEX_ALL
        );
        let influx_query = InfluxRQuery::new(&influx_query_str);

        // the expected JSon type resulting from the query
        #[derive(Deserialize, Debug)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            timestamp: String,
        }
        debug!("Get all entries with Influx query: {}", influx_query_str);
        match self.client.json_query(influx_query).await {
            Ok(mut query_result) => {
                while !query_result.results.is_empty() {
                    match query_result.deserialize_next::<ZenohPoint>() {
                        Ok(retn) => {
                            // for each serie
                            for serie in retn.series {
                                // get the key expression from the serie name
                                match self.keyexpr_from_serie(&serie.name) {
                                    Ok(ke) => {
                                        debug!(
                                            "Replying {} values for {:?}",
                                            serie.values.len(),
                                            ke
                                        );
                                        // for each point in the serie
                                        for zpoint in serie.values {
                                            // get the timestamp (ignore the point if failing)
                                            match Timestamp::from_str(&zpoint.timestamp) {
                                                Ok(timestamp) => {
                                                    result.push((ke.clone(), timestamp))
                                                }
                                                Err(e) => warn!(
                                                    r#"Failed to decode zenoh Timestamp from Influx point {} with timestamp="{}": {:?}"#,
                                                    serie.name, zpoint.timestamp, e
                                                ),
                                            };
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error replying with serie '{}' : {}", serie.name, e)
                                    }
                                };
                            }
                        }
                        Err(e) => {
                            bail!(
                                "Failed to parse result of InfluxDB query '{}': {}",
                                influx_query_str,
                                e
                            )
                        }
                    }
                }
                Ok(result)
            }
            Err(e) => bail!(
                "Failed to query InfluxDb with '{}' : {}",
                influx_query_str,
                e
            ),
        }
    }
    */
}


fn get_db_id(config: StorageConfig)->ZResult<String>{
    let db =  match config.volume_cfg.get(PROP_STORAGE_DB){
        Some(db)=> return Ok(db.to_string()),
        _=> bail!("no db was found"),
       };
}

impl Drop for InfluxDbStorage {

    fn drop(&mut self) {
        debug!("Closing InfluxDB storage");
        let db: String = get_db_id(self.config).expect("didn't get db name");
        match self.on_closure {
            OnClosure::DropDb => {
                task::block_on(async move {

                    debug!("Close InfluxDB storage, dropping database {}", db);
                    let res = self.admin_client.delete_bucket(&db);
                    if let Err(e) = self.admin_client.delete_bucket(&db).await {
                        error!("Failed to drop InfluxDb database '{}' : {}", db, e)
                    }
                });
            }
            OnClosure::DropSeries => {
                task::block_on(async move {
                    debug!(
                        "Close InfluxDB storage, dropping all series from database {}",
                        db
                    );
                    //let query = InfluxRQuery::new("DROP SERIES FROM /.*/");
                    let start = NaiveDateTime::MIN;
                    let stop = NaiveDateTime::MAX;
                    if let Err(e) = self.client.delete(&db,start,stop,None).await {
                        error!(
                            "Failed to drop all series from InfluxDb database '{}' : {}",
                            db, e
                        )
                    }
                });
            }
            OnClosure::DoNothing => {
                debug!(
                    "Close InfluxDB storage, keeping database {} as it is",db
                );
            }
        }
    }
}

// Scheduled dropping of a measurement after a timeout, if it's empty
struct TimedMeasurementDrop {
    client: Client,
    measurement: String,
}

#[async_trait]
impl Timed for TimedMeasurementDrop {
    async fn run(&mut self) {
        #[derive(Deserialize, Debug, PartialEq)]
        struct QueryResult {
            kind: String,
        }

        let db = "db_name";

       

        //let db: String = get_db_id(self.client.config).expect("didn't get db name");
        // check if there is at least 1 point without "DEL" kind in the measurement
        let data_range = "-10000m";
        let qs = format!(
            "from(bucketID: \"{}\")
                                        |> range(start: {})
                                        |> filter(fn: (r) => r._measurement == \"{}\")
                                        |> filter(fn: (r) => r[kind] == DEL)
                                        |> last()
                                    ",
            db, data_range, "cpu"
        );

        let query = Query::new(qs);
        
        let res: Vec<ZenohPoint> = match self.client.query::<ZenohPoint>(Some(query)).await{
                Ok(val) => val.clone(),
                Err(e) => {
                    warn!(
                        "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                        self.measurement, e
                    );
                }
            };
        println!("{:?}", res);

        let query = InfluxRQuery::new(format!(
            r#"SELECT "kind" FROM "{}" WHERE kind!='DEL' LIMIT 1"#,
            self.measurement
        ));
        
        match self.client.json_query(query).await {
            Ok(mut result) => match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() {
                        debug!("Measurement {} contains new values inserted after deletion; don't drop it", self.measurement);
                        return;
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                        self.measurement, e
                    );
                }
            },
            Err(e) => {
                warn!(
                    "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                    self.measurement, e
                );
                return;
            }
        }

        // drop the measurement from the bucket
        let query = InfluxRQuery::new(format!(r#"DROP MEASUREMENT "{}""#, self.measurement));
        debug!(
            "Drop measurement {} after timeout with Influx query: {:?}",
            self.measurement, query
        );
        if let Err(e) = self.client.query(&query).await {
            warn!(
                "Failed to drop measurement '{}' from InfluxDb storage : {}",
                self.measurement, e
            );
        }
    }
}

fn generate_db_name() -> String {
    format!("zenoh_db_{}", Uuid::new_v4().simple())
}

async fn show_databases(client: &Client) -> ZResult<Vec<String>> {
    
    debug!("List databases with Influx query");

    let mut list_of_buckets = Vec::new();
    let  get_buckets= match  client.list_buckets(None).await {
                        Ok(res)=>res.buckets.clone(),
                        Err(e)=> bail!(
                            "Failed to parse list of existing InfluxDb databases : {}",e),
    };
    for buckets in get_buckets{
        let bucket_name = buckets.name;
        let bucket_id = match buckets.id{
            Some(id)=>id.clone(),
            _=>"".to_string(),
        };
        list_of_buckets.push(format!(" name: {} id: {} ",bucket_name,bucket_id));
    }
    Ok(list_of_buckets)
   
}

async fn is_db_existing(client: &Client, db_name: &str) -> ZResult<bool> {
    let request = ListBucketsRequest {
        name: Some(db_name.to_owned()),
        ..ListBucketsRequest::default()
    };
    let res = client.list_buckets(Some(request)).await;
    match res {
        Ok(dbs) => {
            if dbs.buckets.len() > 0 {
                return Ok(true);
            } else {
                return Ok(false);
            }
        }
        Err(_) => return Ok(false),
    };
}

async fn create_db(client: &Client, org_id: &str, db: &str) -> ZResult<bool> {
    let result = client
        .create_bucket(Some(PostBucketRequest::new(
            org_id.to_owned(),
            db.to_owned(),
        )))
        .await;

    match result {
        Ok(_) => return Ok(true),
        Err(_) => return Ok(false), //can post error here
    }
}

// Returns an InfluxDB regex (see https://docs.influxdata.com/influxdb/v1.8/query_language/explore-data/#regular-expressions)
// corresponding to the list of path expressions. I.e.:
// Replace "**" with ".*", "*" with "[^\/]*"  and "/" with "\/".
// Concat each with "|", and surround the result with '/^' and '$/'.
fn key_exprs_to_influx_regex(path_exprs: &[&keyexpr]) -> String {
    let mut result = String::with_capacity(2 * path_exprs[0].len());
    result.push_str("/^");
    for (i, path_expr) in path_exprs.iter().enumerate() {
        if i != 0 {
            result.push('|');
        }
        let mut chars = path_expr.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if let Some(c2) = chars.peek() {
                        if c2 == &'*' {
                            result.push_str(".*");
                            chars.next();
                        } else {
                            result.push_str(".*")
                        }
                    }
                }
                '/' => result.push_str(r"\/"),
                _ => result.push(c),
            }
        }
    }
    result.push_str("$/");
    result
}

fn clauses_from_parameters(p: &str) -> ZResult<String> {
    use zenoh::selector::{TimeBound, TimeRange};
    let time_range = p.time_range()?;
    let mut result = String::with_capacity(256);
    result.push_str("WHERE kind!='DEL'");
    match time_range {
        Some(TimeRange(start, stop)) => {
            match start {
                TimeBound::Inclusive(t) => {
                    result.push_str(" AND time >= ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Exclusive(t) => {
                    result.push_str(" AND time > ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Unbounded => {}
            }
            match stop {
                TimeBound::Inclusive(t) => {
                    result.push_str(" AND time <= ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Exclusive(t) => {
                    result.push_str(" AND time < ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Unbounded => {}
            }
        }
        None => {
            //No time selection, return only latest values
            result.push_str(" ORDER BY time DESC LIMIT 1");
        }
    }
    Ok(result)
}

fn write_timeexpr(s: &mut String, t: TimeExpr) {
    use humantime::format_rfc3339;
    use std::fmt::Write;
    match t {
        TimeExpr::Fixed(t) => write!(s, "'{}'", format_rfc3339(t)),
        TimeExpr::Now { offset_secs } => write!(s, "now(){offset_secs:+}s"),
    }
    .unwrap()
}
