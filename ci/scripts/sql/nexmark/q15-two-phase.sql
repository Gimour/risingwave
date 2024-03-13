-- noinspection SqlNoDataSourceInspectionForFile
-- noinspection SqlResolveForFile
SET rw_force_split_distinct_agg = true;
SET rw_force_two_phase_agg = true;
CREATE SINK nexmark_q15_two_phase AS
SELECT to_char(date_time, 'YYYY-MM-DD')                                          as "day",
       count(*)                                                                  AS total_bids,
       count(*) filter (where price < 10000)                                     AS rank1_bids,
       count(*) filter (where price >= 10000 and price < 1000000)                AS rank2_bids,
       count(*) filter (where price >= 1000000)                                  AS rank3_bids,
       count(distinct bidder)                                                    AS total_bidders,
       count(distinct bidder) filter (where price < 10000)                       AS rank1_bidders,
       count(distinct bidder) filter (where price >= 10000 and price < 1000000)  AS rank2_bidders,
       count(distinct bidder) filter (where price >= 1000000)                    AS rank3_bidders,
       count(distinct auction)                                                   AS total_auctions,
       count(distinct auction) filter (where price < 10000)                      AS rank1_auctions,
       count(distinct auction) filter (where price >= 10000 and price < 1000000) AS rank2_auctions,
       count(distinct auction) filter (where price >= 1000000)                   AS rank3_auctions
FROM bid
GROUP BY to_char(date_time, 'YYYY-MM-DD')
WITH ( connector = 'blackhole', type = 'append-only', force_append_only = 'true');
