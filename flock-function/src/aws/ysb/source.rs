// Copyright (c) 2020-present, UMD Database Group.
//
// This program is free software: you can use, redistribute, and/or modify
// it under the terms of the GNU Affero General Public License, version 3
// or later ("AGPL"), as published by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
// FITNESS FOR A PARTICULAR PURPOSE.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

//! The entry point for the NEXMark benchmark on cloud functions.

use crate::window::*;
use flock::prelude::*;
use log::info;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

/// The endpoint of the data source generator function invocation. The data
/// source generator function is responsible for generating the data packets for
/// the query no matter what type of query it is.
///
/// # Arguments
/// * `ctx` - The runtime context of the function.
/// * `payload` - The payload of the function.
///
/// # Returns
/// A JSON object that contains the return value of the function invocation.
pub async fn handler(ctx: &ExecutionContext, payload: Payload) -> Result<Value> {
    // Copy data source from the payload.
    let mut source = match payload.datasource.clone() {
        DataSource::YSBEvent(source) => source,
        _ => unreachable!(),
    };

    // Each source function is a data generator.
    let gen = source.config.get_as_or("threads", 1);
    let sec = source.config.get_as_or("seconds", 10);
    let eps = source.config.get_as_or("events-per-second", 1000);

    source.config.insert("threads", format!("{}", 1));
    source
        .config
        .insert("events-per-second", format!("{}", eps / gen));
    assert!(eps / gen > 0);

    let events = Arc::new(source.generate_data()?);

    info!("Starting YSB Benchmark.");
    info!("{:?}", source);
    info!("[OK] Generate YSB events.");

    if let Window::Tumbling(Schedule::Seconds(window_size)) = source.window {
        tumbling_window_tasks(payload, events, sec, window_size).await?;
    } else {
        unreachable!();
    }

    Ok(json!({"name": &ctx.name, "type": "ysb_bench".to_string()}))
}
