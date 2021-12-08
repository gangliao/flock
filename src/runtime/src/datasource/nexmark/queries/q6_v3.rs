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

#[allow(dead_code)]
fn main() {}

#[cfg(test)]
mod tests {
    use crate::datasource::nexmark::event::{Auction, Bid, Date};
    use crate::datasource::nexmark::NexMarkSource;
    use crate::error::Result;
    use crate::executor::plan::physical_plan;
    use crate::query::StreamWindow;
    use arrow::array::UInt64Array;
    use arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use datafusion::physical_plan::expressions::Column;
    use datafusion::physical_plan::limit::truncate_batch;
    use datafusion::physical_plan::memory::MemoryExec;
    use datafusion::physical_plan::repartition::RepartitionExec;
    use datafusion::physical_plan::{collect, collect_partitioned};
    use datafusion::physical_plan::{ExecutionPlan, Partitioning};
    use futures::stream::StreamExt;
    use std::sync::Arc;

    #[tokio::test]
    async fn local_query_6_v3() -> Result<()> {
        // benchmark configuration
        let seconds = 2;
        let threads = 1;
        let event_per_second = 1000;
        let nex = NexMarkSource::new(
            seconds,
            threads,
            event_per_second,
            StreamWindow::ElementWise,
        );

        // data source generation
        let events = nex.generate_data()?;

        /*
        let sql = indoc! {"
            SELECT seller, Avg(final)
            FROM   (SELECT ROW_NUMBER() OVER ( PARTITION BY seller ORDER BY date_time DESC) AS row, seller, final
                    FROM   (SELECT  seller, Max(price) AS final, Max(b_date_time) AS date_time
                            FROM auction INNER JOIN bid ON a_id = auction
                            WHERE  b_date_time BETWEEN a_date_time AND expires
                            GROUP  BY a_id,
                                    seller) AS Q) AS R
            WHERE  row <= 10
            GROUP  BY seller;
        "};*/

        let sql = concat!(
            "select seller, avg(price) ",
            "from ( ",
                "select seller, price, b_date_time, rank() over (partition by seller order by b_date_time DESC) time_rank ",
                "from ( ",
                    "SELECT seller, a_id, price, b_date_time, rank() over (partition by a_id order by price DESC) price_rank ",
                    "FROM auction INNER JOIN bid ON a_id = auction ",
                    "WHERE b_date_time between a_date_time and expires ",
                    "ORDER by a_id, price DESC ",
                ") ",
                "where price_rank = 1 ",
            ") ",
            "where time_rank <= 10 ",
            "group by seller ",
            "order by seller"
        );

        let auction_schema = Arc::new(Auction::schema());
        let bid_schema = Arc::new(Bid::schema());

        // sequential processing
        for i in 0..seconds {
            // events to record batches
            let am = events.auctions.get(&Date::new(i)).unwrap();
            let (auctions, _) = am.get(&0).unwrap();
            let auctions_batches = NexMarkSource::to_batch(&auctions, auction_schema.clone());

            let bm = events.bids.get(&Date::new(i)).unwrap();
            let (bids, _) = bm.get(&0).unwrap();
            let bids_batches = NexMarkSource::to_batch(&bids, bid_schema.clone());

            // register memory tables
            let mut ctx = datafusion::execution::context::ExecutionContext::new();
            let auction_table = MemTable::try_new(auction_schema.clone(), vec![auctions_batches])?;
            ctx.register_table("auction", Arc::new(auction_table))?;

            let bid_table = MemTable::try_new(bid_schema.clone(), vec![bids_batches])?;
            ctx.register_table("bid", Arc::new(bid_table))?;

            // optimize query plan and execute it
            let plan = physical_plan(&mut ctx, &sql)?;
            let output_partitions = collect(plan).await?;

            // show output
            let formatted = arrow::util::pretty::pretty_format_batches(&output_partitions).unwrap();
            println!("{}", formatted);
        }

        Ok(())
    }
}
