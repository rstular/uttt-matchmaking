extern crate serde_json;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::BufReader;
use std::process;

#[derive(Serialize, Deserialize)]
pub struct ConfigStruct {
    pub listen_addr: String,
    pub listen_port: u16,
    pub listen_backlog: i32,
    pub keepalive_interval: u64,
    pub keepalive_retries: u32,
}

const DEFAULT_CONFIG_PATH: &str = "config.json";

lazy_static! {
    pub static ref CLI_ARGS: Vec<String> = env::args().collect();
    static ref CONFIG_PATH: String = {
        if CLI_ARGS.len() < 2 {
            DEFAULT_CONFIG_PATH.to_string()
        } else {
            CLI_ARGS[1].to_string()
        }
    };
}

lazy_static! {
    pub static ref CONFIG_FILE: ConfigStruct = {
        info!("Loading configuration");

        let file = match File::open(CONFIG_PATH.as_str()) {
            Ok(f) => f,
            Err(error) => {
                error!(
                    "Could not read configuration file from: '{}' ({})",
                    CONFIG_PATH.as_str(),
                    error.to_string()
                );
                process::exit(-1)
            }
        };

        match serde_json::from_reader(BufReader::new(file)) {
            Ok(data) => data,
            Err(error) => {
                error!(
                    "Could not parse configuration file from: '{}' ({})",
                    CONFIG_PATH.as_str(),
                    error.to_string()
                );
                process::exit(-1)
            }
        }
    };
}
