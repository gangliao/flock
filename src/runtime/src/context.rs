// Copyright (c) 2020-2021, UMD Database Group. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! When the lambda function is called for the first time, it deserializes the
//! corresponding execution context from the cloud environment variable.

use super::datasource::DataSource;
use super::encoding::Encoding;
use crate::error::{Result, SquirtleError};
use arrow::record_batch::RecordBatch;
use datafusion::physical_plan::collect;
use datafusion::physical_plan::memory::MemoryExec;
use datafusion::physical_plan::ExecutionPlan;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;

type CloudFunctionName = String;
type GroupSize = u8;

/// Cloud environment context is a wrapper to support compression and
/// serialization.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CloudEnvironment {
    /// Lambda execution context.
    /// `context` is the serialized version of `ExecutionContext`.
    #[serde(with = "serde_bytes")]
    pub context:  Vec<u8>,
    /// Compress `ExecutionContext` to guarantee the total size
    /// of all environment variables doesn't exceed 4 KB.
    pub encoding: Encoding,
}

/// Next lambda function call.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum CloudFunction {
    /// The next function name with concurrency > 1.
    ///
    /// If the next call type is `Solo`, then the name it contains is the lambda
    /// function.
    Solo(CloudFunctionName),
    /// The next function name with concurrency = 1. To cope with the speed
    /// and volume of data processed, the system creates a function group that
    /// contains multiple functions (names) with the same function code. When
    /// traffic increases dramatically, each query can call a function with
    /// the same code/binary but with different names to avoid delays.
    ///
    /// If the next call type is `Chorus`, then the current function will pick
    /// one of function names from the group as the next call according to a
    /// certain filtering strategy.
    ///
    /// The naming rule is:
    /// If the system picks `i` from the collection [0..`GroupSize`], then the
    /// next call is `CloudFunctionName`-`i`.
    Chorus((CloudFunctionName, GroupSize)),
    /// There is no subsequent call to the cloud function at the end.
    /// TODO: This function must include data sink operation.
    None,
}

impl Default for CloudFunction {
    fn default() -> Self {
        CloudFunction::None
    }
}

/// Lambda execution context.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecutionContext {
    /// The physical sub-plan.
    pub plan:       Arc<dyn ExecutionPlan>,
    /// Cloud Function name in the current execution context.
    ///
    /// |      Cloud Function Naming Convention       |
    /// |---------------------------------------------|
    /// |  query code  -   plan index   -  timestamp  |
    ///
    /// - query code: the cryptographic hash digest of a query produced by
    ///   BLAKE2b ([RFC 7693](https://tools.ietf.org/html/rfc7693)).
    ///
    /// - plan index: the 2-digit number [00-99] indicates the index of the
    ///   subplan of the current query in the dag.
    ///
    /// - timestamp: the time guarantees that the same query can be
    ///   distinguished.
    ///   ISO 8601 date and time format:
    ///   <https://www.iso.org/iso-8601-date-and-time-format.html>
    ///
    /// # Example
    ///
    /// The following is the name of one cloud function generated by the query
    /// at a certain moment.
    ///
    /// SX72HzqFz1Qij4bP-00-2021-01-28T19:27:50.298504836Z
    pub name:       CloudFunctionName,
    /// Lambda function name(s) for next invocation(s).
    pub next:       CloudFunction,
    /// Data source where data that is being used originates from.
    pub datasource: DataSource,
}

impl PartialEq for ExecutionContext {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.next == other.next
            && self.datasource == other.datasource
            && serde_json::to_string(&self.plan).unwrap()
                == serde_json::to_string(&other.plan).unwrap()
    }
}

impl ExecutionContext {
    /// Returns `plan` as a mutable reference.
    pub fn plan(&mut self) -> &mut Arc<dyn ExecutionPlan> {
        &mut self.plan
    }

    /// Executes the physical plan.
    /// `execute` must be called after the execution of `feed_one_source` or
    /// `feed_two_source`.
    pub async fn execute(&mut self) -> Result<Vec<RecordBatch>> {
        match collect(self.plan().clone()).await {
            Ok(b) => Ok(b),
            Err(e) => Err(SquirtleError::Plan(format!(
                "{}. Failed to execute the plan '{:?}'",
                e, self.plan
            ))),
        }
    }

    /// Serializes `ExecutionContext` from client-side.
    pub fn marshal(&self, encoding: Encoding) -> String {
        match encoding {
            Encoding::Snappy => {
                let encoded: Vec<u8> = serde_json::to_vec(&self).unwrap();
                serde_json::to_string(&CloudEnvironment {
                    context: encoding.encoder(&encoded),
                    encoding,
                })
                .unwrap()
            }
            Encoding::None => serde_json::to_string(&CloudEnvironment {
                context: serde_json::to_vec(&self).unwrap(),
                encoding,
            })
            .unwrap(),
            _ => unimplemented!(),
        }
    }

    /// Deserializes `ExecutionContext` from cloud-side.
    pub fn unmarshal(s: &str) -> ExecutionContext {
        let env: CloudEnvironment = serde_json::from_str(&s).unwrap();

        match env.encoding {
            Encoding::Snappy => {
                let encoded = env.encoding.decoder(&env.context);
                serde_json::from_slice(&encoded).unwrap()
            }
            Encoding::None => serde_json::from_slice(&env.context).unwrap(),
            _ => unimplemented!(),
        }
    }

    /// Feed one data source to the execution plan.
    pub fn feed_one_source(&mut self, partitions: &Vec<Vec<RecordBatch>>) {
        // Breadth-first search
        let mut queue = VecDeque::new();
        queue.push_front(self.plan().clone());

        while !queue.is_empty() {
            let mut p = queue.pop_front().unwrap();
            if p.children().is_empty() {
                unsafe {
                    Arc::get_mut_unchecked(&mut p)
                        .as_mut_any()
                        .downcast_mut::<MemoryExec>()
                        .unwrap()
                        .set_partitions(&partitions);
                }
                break;
            }

            p.children()
                .iter()
                .enumerate()
                .for_each(|(i, _)| queue.push_back(p.children()[i].clone()));
        }
    }

    /// Feed two data sources to the execution plan like join two tables.
    pub fn feed_two_source(&mut self, left: &Vec<Vec<RecordBatch>>, right: &Vec<Vec<RecordBatch>>) {
        // Breadth-first search
        let mut queue = VecDeque::new();
        queue.push_front(self.plan().clone());

        while !queue.is_empty() {
            let mut p = queue.pop_front().unwrap();
            if p.children().is_empty() {
                // Schema comparsion
                for partition in &[&left, &right] {
                    if p.schema() == partition[0][0].schema() {
                        unsafe {
                            Arc::get_mut_unchecked(&mut p)
                                .as_mut_any()
                                .downcast_mut::<MemoryExec>()
                                .unwrap()
                                .set_partitions(&partition);
                        }
                    }
                }
            }

            p.children()
                .iter()
                .enumerate()
                .for_each(|(i, _)| queue.push_back(p.children()[i].clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;

    use crate::datasource::kinesis;
    use aws_lambda_events::event::kinesis::KinesisEvent;
    use datafusion::datasource::MemTable;
    use datafusion::physical_plan::collect;

    use arrow::array::*;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    #[tokio::test]
    async fn lambda_context_marshal() -> Result<()> {
        let plan = r#"{"execution_plan":"coalesce_batches_exec","input":{"execution_plan":"memory_exec","schema":{"fields":[{"name":"c1","data_type":"Int64","nullable":true,"dict_id":0,"dict_is_ordered":false},{"name":"c2","data_type":"Float64","nullable":true,"dict_id":0,"dict_is_ordered":false},{"name":"c3","data_type":"Utf8","nullable":true,"dict_id":0,"dict_is_ordered":false}],"metadata":{}},"projection":null},"target_batch_size":16384}"#;
        let name = "hello".to_owned();
        let next =
            CloudFunction::Solo("SX72HzqFz1Qij4bP-00-2021-01-28T19:27:50.298504836Z".to_owned());
        let datasource = DataSource::Payload;

        let plan: Arc<dyn ExecutionPlan> = serde_json::from_str(&plan)?;
        let lambda_context = ExecutionContext {
            plan,
            name,
            next,
            datasource,
        };

        let json = lambda_context.marshal(Encoding::Snappy);
        let de_json = ExecutionContext::unmarshal(&json);
        assert_eq!(lambda_context, de_json);

        Ok(())
    }

    #[tokio::test]
    async fn feed_one_source() -> Result<()> {
        let input = include_str!("../../../examples/lambda/example-kinesis-event.json");
        let input: KinesisEvent = serde_json::from_str(input).unwrap();

        let record_batch = kinesis::to_batch(input);
        let partitions = vec![vec![record_batch]];

        let mut ctx = datafusion::execution::context::ExecutionContext::new();
        let provider = MemTable::try_new(partitions[0][0].schema(), partitions.clone())?;

        ctx.register_table("test", Box::new(provider));

        let sql = "SELECT MAX(c1), MIN(c2), c3 FROM test WHERE c2 < 99 GROUP BY c3";
        let logical_plan = ctx.create_logical_plan(&sql)?;
        let logical_plan = ctx.optimize(&logical_plan)?;
        let physical_plan = ctx.create_physical_plan(&logical_plan)?;

        // Serialize the physical plan and skip its record batches
        let plan = serde_json::to_string(&physical_plan)?;

        // Deserialize the physical plan that doesn't contain record batches
        let plan: Arc<dyn ExecutionPlan> = serde_json::from_str(&plan)?;

        // Feed record batches back to the plan
        let mut ctx = ExecutionContext {
            plan,
            name: "test".to_string(),
            next: CloudFunction::None,
            datasource: DataSource::UnknownEvent,
        };
        ctx.feed_one_source(&partitions);

        let batches = collect(ctx.plan.clone()).await?;
        let formatted = arrow::util::pretty::pretty_format_batches(&batches).unwrap();
        let actual_lines: Vec<&str> = formatted.trim().lines().collect();

        let expected = vec![
            "+---------+---------+----+",
            "| MAX(c1) | MIN(c2) | c3 |",
            "+---------+---------+----+",
            "| 90      | 92.1    | a  |",
            "+---------+---------+----+",
        ];

        assert_eq!(expected, actual_lines);

        Ok(())
    }

    #[tokio::test]
    async fn feed_two_source() -> Result<()> {
        let schema1 = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Utf8, false),
            Field::new("b", DataType::Int32, false),
        ]));
        let schema2 = Arc::new(Schema::new(vec![
            Field::new("c", DataType::Utf8, false),
            Field::new("d", DataType::Int32, false),
        ]));

        // define data.
        let batch1 = RecordBatch::try_new(
            schema1.clone(),
            vec![
                Arc::new(StringArray::from(vec!["a", "b", "c", "d"])),
                Arc::new(Int32Array::from(vec![1, 10, 10, 100])),
            ],
        )?;
        // define data.
        let batch2 = RecordBatch::try_new(
            schema2.clone(),
            vec![
                Arc::new(StringArray::from(vec!["a", "b", "c", "d"])),
                Arc::new(Int32Array::from(vec![1, 10, 10, 100])),
            ],
        )?;

        let partitions1 = vec![vec![batch1]];
        let partitions2 = vec![vec![batch2]];

        let mut ctx = datafusion::execution::context::ExecutionContext::new();

        let table1 = MemTable::try_new(schema1, partitions1.clone())?;
        let table2 = MemTable::try_new(schema2, partitions2.clone())?;

        ctx.register_table("t1", Box::new(table1));
        ctx.register_table("t2", Box::new(table2));

        let sql = concat!(
            "SELECT a, b, d ",
            "FROM t1 JOIN t2 ON a = c ",
            "ORDER BY b ASC ",
            "LIMIT 3"
        );

        let logical_plan = ctx.create_logical_plan(&sql)?;
        let logical_plan = ctx.optimize(&logical_plan)?;
        let physical_plan = ctx.create_physical_plan(&logical_plan)?;

        // Serialize the physical plan and skip its record batches
        let plan = serde_json::to_string(&physical_plan)?;

        // Deserialize the physical plan that doesn't contain record batches
        let plan: Arc<dyn ExecutionPlan> = serde_json::from_str(&plan)?;

        // Feed record batches back to the plan
        let mut ctx = ExecutionContext {
            plan,
            name: "test".to_string(),
            next: CloudFunction::None,
            datasource: DataSource::UnknownEvent,
        };
        ctx.feed_two_source(&partitions1, &partitions2);

        let batches = collect(ctx.plan.clone()).await?;
        let formatted = arrow::util::pretty::pretty_format_batches(&batches).unwrap();
        let actual_lines: Vec<&str> = formatted.trim().lines().collect();

        let expected = vec![
            "+---+----+----+",
            "| a | b  | d  |",
            "+---+----+----+",
            "| a | 1  | 1  |",
            "| b | 10 | 10 |",
            "| c | 10 | 10 |",
            "+---+----+----+",
        ];

        assert_eq!(expected, actual_lines);

        Ok(())
    }
}
