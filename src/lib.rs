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
//use influxdb;//::{Client, ReadQuery as InfluxRQuery, Timestamp as InfluxTimestamp, WriteQuery as InfluxWQuery};

use futures::prelude::*;
use influxdb2::api::buckets::ListBucketsRequest;
use influxdb2::models::{DataPoint, PostBucketRequest};
use influxdb2::{Client, RequestError};

use chrono::{NaiveDateTime, Utc};
use influxdb2::models::Query;
use influxdb2::FromDataPoint;

use log::{debug, error, warn};
use std::convert::{TryFrom, TryInto};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};
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
pub const PROP_BACKEND_ORG_ID: &str = "org_id";

pub const PROP_BACKEND_INFLUXDB_VERSION: &str = "influxdb_version";
pub const PROP_TOKEN: &str = "token";

// Properties used by the Storage
pub const PROP_STORAGE_DB: &str = "db";
pub const PROP_STORAGE_DB_ID: &str = "db_id";
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

fn extract_credentials(config: Config) -> ZResult<Option<InfluxDbCredentials>> {
    match (
        get_private_conf(config, PROP_BACKEND_ORG_ID)?,
        get_private_conf(config, PROP_TOKEN)?,
    ) {
        (Some(org_id), Some(token)) => Ok(Some(InfluxDbCredentials::Creds(
            org_id.clone(),
            token.clone(),
        ))),
        _ => {
            error!("Couldn't get token and org");
            bail!(
                "Properties `{}` and `{}` must exist",
                PROP_BACKEND_ORG_ID,
                PROP_TOKEN
            );
        }
    }
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

    let url = match config.rest.get(PROP_BACKEND_URL) {
        Some(serde_json::Value::String(url)) => url.clone(),
        _ => {
            bail!(
                "Mandatory property `{}` for InfluxDb Backend must be a string",
                PROP_BACKEND_URL
            )
        }
    };

    // The InfluxDB "admin" client that will be used for creating and dropping buckets (create/drop databases)
    let mut admin_client: Client;

    let credentials = extract_credentials(&config.rest)?;
    match &credentials {
        Some(InfluxDbCredentials::Creds(org_id, token)) => {
            admin_client = Client::new(url.clone(), org_id.clone(), token); //what if it panics (the library unwraps this)

            match async_std::task::block_on(async { admin_client.ready().await }) {
                Ok(res) => {
                    if !res {
                        bail!("Influxdb server is not ready! ")
                    }
                }
                Err(e) => bail!("Failed to create InfluxDb Volume : {:?}", e),
            }

            Ok(Box::new(InfluxDbBackend {
                admin_status: config,
                admin_client,
                credentials,
            }))
        }
        None => {
            bail!("Admin creds not provided. Can't proceed without them.");
        }
    }

    // Check connectivity to InfluxDB

    //if everything above works, the volume is created here
}

// Enum to take care of different credential forms
// Keep in mind that influx v1 allow access without credentials
#[derive(Debug)]
enum InfluxDbCredentials {
    Creds(String, String),
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
        //  let mut client: Client;

        let url = match &self.admin_status.rest.get(PROP_BACKEND_URL) {
            Some(serde_json::Value::String(url)) => url.clone(),
            _ => {
                bail!(
                    "Mandatory property `{}` for InfluxDb Backend must be a string",
                    PROP_BACKEND_URL
                )
            }
        };

        //   let url = get_url(self.admin_status.clone()).expect("url not found");
        let mut org_id: &str;
        let mut token: &str;

        // Use token creds if specified in storage's volume config
        // create user with token creds if user exists, if no token, then abort

        let credentials = extract_credentials(volume_cfg)?;
        match &credentials {
            Some(InfluxDbCredentials::Creds(client_org, client_token)) => {
                org_id = client_org;
                token = client_token;
            }
            _ => bail!("No credentials specified to access database '{}'", db),
        }
        let client = Client::new(url.clone(), org_id.clone(), token.clone()); //what if it panics (the library unwraps this)
                                                                              // Check if the database exists (using storage credentials)
        match async_std::task::block_on(async { is_db_existing(&client, &db).await }) {
            Ok(res) => {
                if !res && createdb {
                    // try to create db using user credentials
                    match async_std::task::block_on(async {
                        create_db(&self.admin_client, org_id, &db).await
                    }) {
                        Ok(res) => {
                            if !res {
                                bail!("Database '{}' wasnt't created in InfluxDb", db)
                            }
                        }
                        Err(e) => bail!("Failed to create InfluxDb Storage : {:?}", e),
                    }
                }
            }
            Err(e) => bail!("Failed to create InfluxDb Storage : {:?}", e),
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

        //creating empty client. can we use admin client values  directly here??
        let mut admin_org_id = "";
        let mut admin_token = "";
        match &self.credentials {
            Some(InfluxDbCredentials::Creds(org_id, token)) => {
                (admin_org_id, admin_token) = (&org_id, &token);
            }
            None => {}
        }

        let admin_client = Client::new(url.clone(), admin_org_id, admin_token); //what if it panics (the library unwraps this)
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

    // fn keyexpr_from_serie(&self, serie_name: &str) -> ZResult<Option<OwnedKeyExpr>> {
    //     if serie_name.eq(NONE_KEY) {
    //         Ok(None)
    //     } else {
    //         match OwnedKeyExpr::from_str(serie_name) {
    //             Ok(key) => Ok(Some(key)),
    //             Err(e) => Err(format!("{}", e).into()),
    //         }
    //     }
    // }
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
        // encode the value as a string to be stored in InfluxDB, converting to base64 if the buffer is not a UTF-8 string
        let (base64, strvalue) = match String::from_utf8(value.payload.contiguous().into_owned()) {
            Ok(s) => (false, s),
            Err(err) => (true, b64_std_engine.encode(err.into_bytes())),
        };

        let db = match self.config.volume_cfg.get(PROP_STORAGE_DB) {
            Some(serde_json::Value::String(s)) => s.to_string(),
            _ => bail!("no db was found"),
        };
        // Note: tags are stored as strings in InfluxDB, while fileds are typed.
        // For simpler/faster deserialization, we store encoding, timestamp and base64 as fields.
        // while the kind is stored as a tag to be indexed by InfluxDB and have faster queries on it.
        let zenoh_point = vec![DataPoint::builder(measurement.clone())
            .tag("kind", "PUT")
            .tag("ztimestamp", timestamp.to_string())
            .field(
                "encoding_prefix",
                u8::from(*value.encoding.prefix()).to_string(),
            ) //issue with this data-type, converting it to string for now, will see later
            .field("encoding_suffix", value.encoding.suffix())
            .field("base64", base64)
            .field("value", strvalue)
            .build()
            .expect("error in building the write query")];

        match async_std::task::block_on(async {
            self.client.write(&db, stream::iter(zenoh_point)).await
        }) {
            Ok(_) => Ok(StorageInsertionResult::Inserted),
            Err(e) => bail!(
                "Failed to put Value for {:?} in InfluxDb storage : {}",
                measurement,
                e
            ),
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

        let db = match self.config.volume_cfg.get(PROP_STORAGE_DB) {
            Some(serde_json::Value::String(s)) => s.to_string(),
            _ => bail!("no db was found"),
        };
        let start = NaiveDateTime::from_timestamp_millis(0).unwrap();
        let deletion_time =
            NaiveDateTime::from_timestamp_millis(timestamp.get_time().0.try_into().unwrap())
                .unwrap();
        let predicate = None; //can be specified as tag or field values
        debug!("Delete {:?} with Influx query", measurement);
        if let Err(e) = self
            .client
            .delete(&db, start, deletion_time, predicate)
            .await
        {
            bail!(
                "Failed to delete points for measurement '{}' from InfluxDb storage : {}",
                measurement,
                e
            )
        }
        // store a point (with timestamp) with "delete" tag, thus we don't re-introduce an older point later

        let zenoh_point = vec![DataPoint::builder(measurement.clone())
            .tag("kind", "DELETE")
            .tag("ztimestamp", timestamp.to_string())
            .field("encoding_prefix", "") //issue with this data-type, converting it to string for now, will see later
            .field("encoding_suffix", "")
            .field("base64", false)
            .field("value", "")
            .build()
            .expect("error in building the write query")];

        debug!(
            "Mark measurement {} as deleted at time {}",
            measurement.clone(),
            deletion_time
        );
        if let Err(e) = self.client.write(&db, stream::iter(zenoh_point)).await {
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

        let db = match self.config.volume_cfg.get(PROP_STORAGE_DB) {
            Some(serde_json::Value::String(s)) => s.to_string(),
            _ => bail!("no db was found"),
        };

        // the expected JSon type resulting from the query
        // #[derive(Deserialize, Debug)]
        #[derive(Debug, Default, FromDataPoint)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            ztimestamp: String,
            encoding_prefix: String, //is u8 but not supported in v2 so working around that for now
            encoding_suffix: String,
            base64: bool,
            value: String,
        }

        let stop_time = Utc::now().naive_utc();
        let time_range = format!("-{}s", stop_time.timestamp());
        let mut qs: String;

        match time_from_parameters(parameters)? {
            Some((start, stop)) => {
                qs = format!(
                    "from(bucketID: \"{}\")
                                            |> range(start: {}, stop: {})
                                            |> filter(fn: (r) => r._measurement == \"{}\")
                                            |> filter(fn: (r) => r[\"kind\"] == \"PUT\")
                                        ",
                    db, start, stop, measurement
                );
            }
            None => {
                qs = format!(
                    "from(bucketID: \"{}\")
                                            |> range(start: {})
                                            |> filter(fn: (r) => r._measurement == \"{}\")
                                            |> filter(fn: (r) => r[\"kind\"] == \"PUT\")
                                            |> last()
                                        ",
                    db, time_range, measurement
                );
            }
        }

        debug!("Get {:?} with Influx query:{}", key, qs);

        let query = Query::new(qs);
        let mut query_result: Vec<ZenohPoint> = vec![];

        match async_std::task::block_on(async {
            self.client.query::<ZenohPoint>(Some(query)).await
        }) {
            Ok(result) => {
                query_result = result;
            }
            Err(e) => {
                error!("Couldn't get data from database {} with error: {} ", db, e);
            }
        };
        let mut result: Vec<StoredData> = vec![];

        for i in &query_result {
            let ztimestamp = Timestamp::from_str(&i.ztimestamp).expect("didnt parse timestamp");
            result.push(StoredData {
                value: i.value.clone().into(),
                timestamp: ztimestamp,
            });
        }

        Ok(result)
    }

    //putting a stub here to be implemented later
    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>> {
        // let db = match self.config.volume_cfg.get(PROP_STORAGE_DB) {
        //     Some(serde_json::Value::String(s)) => s.to_string(),
        //     _ => bail!("no db was found"),
        // };

        // #[derive(Debug, Default, FromDataPoint)]
        // struct Measurement {
        //     name: String,
        // }
        // let qs = format!("SHOW MEASUREMENTS ON {}", db); //bad query (need to find replacement)
        // let query = Query::new(qs);
        // println!("proceeds till here 1");

        // let r =
        //     self.client.query::<Measurement>(Some(query)).await;
        //
        // println!("proceeds till here 1");

        // let res: Vec<Measurement> = r.expect("got bad query format");
        // println!("this printed something{:?}", res);

        //for stub
        let mut result: Vec<(Option<OwnedKeyExpr>, Timestamp)> = Vec::new();
        let curr_time = zenoh::time::new_reception_timestamp();
        result.push((None, curr_time));
        return Ok(result);
    }
}

fn get_db_id(config: StorageConfig) -> ZResult<String> {
    let db = match config.volume_cfg.get(PROP_STORAGE_DB) {
        Some(serde_json::Value::String(s)) => s.to_string(),
        _ => bail!("no db was found"),
    };
    Ok(db)
}

impl Drop for InfluxDbStorage {
    fn drop(&mut self) {
        debug!("Closing InfluxDB storage");
        let db: String = get_db_id(self.config.clone()).expect("didn't get db name");
        match self.on_closure {
            OnClosure::DropDb => {
                task::block_on(async move {
                    debug!("Close InfluxDB storage, dropping database {}", db);
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
                    let start = NaiveDateTime::MIN;
                    let stop = NaiveDateTime::MAX;
                    if let Err(e) = self.client.delete(&db, start, stop, None).await {
                        error!(
                            "Failed to drop all series from InfluxDb database '{}' : {}",
                            db, e
                        )
                    }
                });
            }
            OnClosure::DoNothing => {
                debug!("Close InfluxDB storage, keeping database {} as it is", db);
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
        //keeping the stub here for implemntation in a future time
        //currently the influxDB library doesn't allow dropping a measurement
    }
}

fn generate_db_name() -> String {
    format!("zenoh_db_{}", Uuid::new_v4().simple())
}

async fn is_db_existing(client: &Client, db_id: &str) -> ZResult<bool> {
    let request = ListBucketsRequest {
        id: Some(db_id.to_owned()),
        ..ListBucketsRequest::default()
    };

    match client.list_buckets(Some(request)).await {
        Ok(dbs) => {
            if !dbs.buckets.is_empty() {
                return Ok(true);
            }
            Ok(false)
        }
        Err(_) => Ok(false),
    }
}

//this should return the DB_ID
async fn create_db(client: &Client, org_id: &str, db: &str) -> ZResult<bool> {
    let result = z_create_bucket(
        client,
        Some(PostBucketRequest::new(org_id.to_owned(), db.to_owned())),
    )
    .await;

    match result {
        Ok(_) => return Ok(true),
        Err(_) => return Ok(false), //can post error here
    }
}

pub async fn z_create_bucket(
    client: &Client,
    // url: String,
    // org_id: String,
    // token: String,
    post_bucket_request: Option<PostBucketRequest>,
) -> Result<(), RequestError> {
    //create new bucket using "admin" permissions
    //send the bucket_id back to the calling fn

    // let mut url = client.base.clone();
    // url.set_path("/api/v2/buckets");
    // let create_bucket_url = url.into();

    // let response = client
    //     .request(Method::POST, &create_bucket_url)
    //     .body(serde_json::to_string(&post_bucket_request.unwrap_or_default()).context(Serializing)?)
    //     .send()
    //     .await
    //     .context(ReqwestProcessing)?;

    // if !response.status().is_success() {
    //     let status = response.status();
    //     let text = response.text().await.context(ReqwestProcessing)?;
    //     Http { status, text }.fail()?;
    // }

    Ok(())
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

fn time_from_parameters(t: &str) -> ZResult<Option<(u64, u64)>> {
    use zenoh::selector::{TimeBound, TimeRange};
    let time_range = t.time_range()?;

    let mut start_time: u64 = 0;
    let mut stop_time: u64 = Utc::now().naive_utc().timestamp().try_into().unwrap();

    match time_range {
        Some(TimeRange(start, stop)) => {
            match start {
                TimeBound::Inclusive(t) => {
                    start_time = calculate_time(t).unwrap();
                }
                TimeBound::Exclusive(t) => {
                    start_time = calculate_time(t).unwrap();
                }
                TimeBound::Unbounded => {}
            }
            match stop {
                TimeBound::Inclusive(t) => {
                    stop_time = calculate_time(t).unwrap();
                }
                TimeBound::Exclusive(t) => {
                    stop_time = calculate_time(t).unwrap();
                }
                TimeBound::Unbounded => {}
            }
        }
        None => {
            return Ok(None);
        }
    }
    Ok(Some((start_time, stop_time)))
}

fn calculate_time(tx: TimeExpr) -> ZResult<u64> {
    //Note: conversions bw f64,i64,u64
    let now_time = Utc::now().naive_utc().timestamp();
    match tx {
        TimeExpr::Fixed(t) => {
            let time_in_secs = t
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();
            Ok(time_in_secs)
        }
        TimeExpr::Now { offset_secs } => Ok((now_time + offset_secs as i64).try_into().unwrap()),
    }
}
