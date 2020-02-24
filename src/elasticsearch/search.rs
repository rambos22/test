use crate::elasticsearch::{Elasticsearch, ElasticsearchError};
use crate::zdbquery::ZDBQuery;
use serde::*;
use serde_json::*;
use std::collections::HashMap;

const SEARCH_FILTER_PATH:&'static str = "_scroll_id,_shards.*,hits.total,hits.max_score,hits.hits._score,hits.hits.fields.*,hits.hits.highlight.*";

pub struct ElasticsearchSearchRequest {
    elasticsearch: Elasticsearch,
    query: ZDBQuery,
}

#[derive(Deserialize)]
pub struct Shards {
    total: Option<u64>,
    successful: Option<u64>,
    skipped: Option<u64>,
    failed: Option<u64>,
}

#[derive(Deserialize)]
pub struct HitsTotal {
    value: u64,
    relation: String,
}

#[derive(Deserialize)]
pub struct Fields {
    zdb_ctid: [u64; 1],

    #[serde(flatten)]
    other: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
pub struct InnerHit {
    #[serde(rename = "_index")]
    index: Option<String>,

    #[serde(rename = "_type")]
    type_: Option<String>,

    #[serde(rename = "_score")]
    score: Option<f64>,

    #[serde(rename = "_id")]
    id: Option<String>,

    fields: Fields,
}

#[derive(Deserialize)]
pub struct Hits {
    total: HitsTotal,
    max_score: Option<f64>,
    hits: Option<Vec<InnerHit>>,
}

#[derive(Deserialize)]
pub struct ElasticsearchSearchResponse {
    #[serde(skip)]
    elasticsearch: Option<Elasticsearch>,
    #[serde(skip)]
    limit: Option<u64>,
    #[serde(skip)]
    offset: Option<u64>,

    #[serde(rename = "_scroll_id")]
    scroll_id: Option<String>,

    #[serde(rename = "_shards")]
    shards: Option<Shards>,

    hits: Option<Hits>,
}

impl ElasticsearchSearchRequest {
    pub fn new(elasticsearch: &Elasticsearch, query: ZDBQuery) -> Self {
        ElasticsearchSearchRequest {
            elasticsearch: elasticsearch.clone(),
            query,
        }
    }

    pub fn execute(self) -> std::result::Result<ElasticsearchSearchResponse, ElasticsearchError> {
        ElasticsearchSearchRequest::initial_search(self.elasticsearch, self.query)
    }

    fn initial_search(
        elasticsearch: Elasticsearch,
        query: ZDBQuery,
    ) -> std::result::Result<ElasticsearchSearchResponse, ElasticsearchError> {
        let mut url = String::new();
        url.push_str(&elasticsearch.base_url());
        url.push_str("/_search");
        url.push_str("?search_type=query_then_fetch");
        url.push_str("&_source=false");
        url.push_str("&scroll=10m");
        url.push_str(&format!("&filter_path={}", SEARCH_FILTER_PATH));
        url.push_str("&stored_fields=_none_");
        url.push_str("&docvalue_fields=zdb_ctid");

        // do we need to track scores?
        let track_scores =
            query.want_score() || query.limit().is_some() || query.min_score().is_some();

        // how should we sort the results?
        let mut sort_json = query.sort_json().cloned();

        // adjust the chunk size we want Elasticsearch to return for us
        // to be that of our limit
        match query.limit() {
            Some(limit) if limit == 0 => {
                // with a limit of zero, we can avoid going to Elasticsearch at all
                // and just return a (mostly) None'd response
                return Ok(ElasticsearchSearchResponse {
                    elasticsearch: None,
                    limit: Some(0),
                    offset: None,
                    scroll_id: None,
                    shards: None,
                    hits: None,
                });
            }
            Some(limit) if limit < 10_000 => {
                url.push_str(&format!("&size={}", limit));
                // if we don't already have a sort_json, create one to
                // order by _score desc
                if sort_json.is_none() {
                    sort_json = Some(json!([{"_score": "desc"}]));
                }
            }
            _ => {
                url.push_str("&size=10000");
            }
        }

        // if we made it this far and never set a sort, we'll hard-code
        // sorting against zdb_ctid asc so that we return rows in heap
        // order, which is much nicer to disk I/O
        if sort_json.is_none() {
            // TODO:  This is about 50% slower on my laptop.
            // sort_json = Some(json!([{"zdb_ctid": "asc"}]))
        }

        #[derive(Serialize)]
        struct Body<'a> {
            track_scores: bool,

            #[serde(skip_serializing_if = "Option::is_none")]
            min_score: Option<f64>,

            #[serde(skip_serializing_if = "Option::is_none")]
            sort: Option<Value>,

            query: &'a Value,
        }

        let body = Body {
            track_scores,
            min_score: query.min_score(),
            sort: sort_json,
            query: query.query_dsl().expect("zdbquery has no QueryDSL"),
        };

        ElasticsearchSearchRequest::get_hits(
            url,
            query.limit(),
            query.offset(),
            elasticsearch,
            json! { body },
        )
    }

    fn scroll(
        elasticsearch: Elasticsearch,
        scroll_id: &str,
    ) -> std::result::Result<ElasticsearchSearchResponse, ElasticsearchError> {
        let mut url = String::new();
        url.push_str(&elasticsearch.options.url);
        url.push_str("_search/scroll");
        url.push_str("?filter_path=");
        url.push_str(SEARCH_FILTER_PATH);

        ElasticsearchSearchRequest::get_hits(
            url,
            None,
            None,
            elasticsearch,
            json! {
                {
                    "scroll": "10m",
                    "scroll_id": scroll_id
                }
            },
        )
    }

    fn get_hits(
        url: String,
        limit: Option<u64>,
        offset: Option<u64>,
        elasticsearch: Elasticsearch,
        body: serde_json::Value,
    ) -> std::result::Result<ElasticsearchSearchResponse, ElasticsearchError> {
        Elasticsearch::execute_request(
            reqwest::Client::new()
                .post(&url)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&body).unwrap()),
            |code, body| {
                let mut response = match serde_json::from_str::<ElasticsearchSearchResponse>(&body)
                {
                    Ok(json) => json,
                    Err(_) => {
                        return Err(ElasticsearchError(Some(code), body));
                    }
                };

                // assign a clone of our ES client to the response too,
                // for future use during iteration
                response.elasticsearch = Some(elasticsearch);
                response.limit = limit;
                response.offset = offset;

                Ok(response)
            },
        )
    }
}

pub struct Scroller {
    receiver: crossbeam::channel::Receiver<(f64, u64)>,
}

impl Scroller {
    fn new(
        elasticsearch: Elasticsearch,
        mut scroll_id: Option<String>,
        iter: std::vec::IntoIter<InnerHit>,
    ) -> Self {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let (scroll_sender, scroll_receiver) = crossbeam::channel::unbounded();

        // go ahead and queue up the results we currently have from the initial search
        scroll_sender
            .send(iter)
            .expect("scroll_sender channel is closed");

        // spawn a thread to continually get the next scroll chunk from Elasticsearch
        // until there's no more to get
        std::thread::spawn(move || {
            std::panic::catch_unwind(|| {
                while let Some(sid) = scroll_id {
                    match ElasticsearchSearchRequest::scroll(elasticsearch.clone(), &sid) {
                        Ok(response) => {
                            scroll_id = response.scroll_id;

                            match response.hits.unwrap().hits {
                                Some(inner_hits) => {
                                    // send the hits across the scroll_sender channel
                                    // so they can be iterated from another thread
                                    scroll_sender
                                        .send(inner_hits.into_iter())
                                        .expect("failed to send iter over scroll_sender");
                                }
                                None => {
                                    break;
                                }
                            }
                        }
                        Err(_) => {
                            break;
                        }
                    }

                    if scroll_id.is_none() {
                        break;
                    }
                }
            })
            .ok();
        });

        // this thread sends the hits back to the main thread across the 'sender/receiver' channel
        // until there's no more to send
        std::thread::spawn(move || {
            std::panic::catch_unwind(|| {
                for itr in scroll_receiver {
                    for hit in itr {
                        sender
                            .send((hit.score.unwrap_or_default(), hit.fields.zdb_ctid[0]))
                            .expect("failed to send hit over sender");
                    }
                }
            })
            .ok();
        });

        Scroller { receiver }
    }

    fn next(&self) -> Option<(f64, u64)> {
        match self.receiver.recv() {
            Ok(tuple) => Some(tuple),
            Err(_) => None,
        }
    }
}

pub struct SearchResponseIntoIter {
    scroller: Option<Scroller>,
    limit: Option<u64>,
    cnt: u64,
}

impl Iterator for SearchResponseIntoIter {
    type Item = (f64, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(limit) = self.limit {
            if self.cnt >= limit {
                // we've reached our limit
                return None;
            }
        }

        let scroller = self.scroller.as_ref().unwrap();
        let item = scroller.next();
        self.cnt += 1;
        item
    }
}

impl IntoIterator for ElasticsearchSearchResponse {
    type Item = (f64, u64);
    type IntoIter = SearchResponseIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        if self.elasticsearch.is_none() {
            SearchResponseIntoIter {
                scroller: None,
                limit: Some(0),
                cnt: 0,
            }
        } else {
            let scroller = Scroller::new(
                self.elasticsearch.expect("no elasticsearch"),
                self.scroll_id,
                self.hits.unwrap().hits.unwrap_or_default().into_iter(),
            );

            // fast forward to our offset -- using the ?from= ES request parameter doesn't work with scroll requests
            if let Some(offset) = self.offset {
                for _ in 0..offset {
                    if scroller.next().is_none() {
                        break;
                    }
                }
            }

            SearchResponseIntoIter {
                scroller: Some(scroller),
                limit: self.limit,
                cnt: 0,
            }
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use pgx::*;

    #[pg_test]
    #[initialize(es = true)]
    fn test_limit_none() {
        Spi::run("CREATE TABLE test_limit AS SELECT * FROM generate_series(1, 10001);");
        Spi::run("CREATE INDEX idxtest_limit ON test_limit USING zombodb ((test_limit.*));");
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_limit WHERE test_limit ==> dsl.match_all(); ",
        )
        .expect("failed to get SPI result");
        assert_eq!(count, 10_001);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_limit_exact() {
        Spi::run("CREATE TABLE test_limit AS SELECT * FROM generate_series(1, 10001);");
        Spi::run("CREATE INDEX idxtest_limit ON test_limit USING zombodb ((test_limit.*));");
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_limit WHERE test_limit ==> dsl.limit(10001, dsl.match_all()); ",
        )
        .expect("failed to get SPI result");
        assert_eq!(count, 10001);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_limit_10() {
        Spi::run("CREATE TABLE test_limit AS SELECT * FROM generate_series(1, 10001);");
        Spi::run("CREATE INDEX idxtest_limit ON test_limit USING zombodb ((test_limit.*));");
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_limit WHERE test_limit ==> dsl.limit(10, dsl.match_all()); ",
        )
        .expect("failed to get SPI result");
        assert_eq!(count, 10);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_limit_0() {
        Spi::run("CREATE TABLE test_limit AS SELECT * FROM generate_series(1, 10001);");
        Spi::run("CREATE INDEX idxtest_limit ON test_limit USING zombodb ((test_limit.*));");
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_limit WHERE test_limit ==> dsl.limit(0, dsl.match_all()); ",
        )
        .expect("failed to get SPI result");
        assert_eq!(count, 0);
    }

    #[pg_test(error = "limit must be positive")]
    #[initialize(es = true)]
    fn test_limit_negative() {
        Spi::run("CREATE TABLE test_limit AS SELECT * FROM generate_series(1, 10001);");
        Spi::run("CREATE INDEX idxtest_limit ON test_limit USING zombodb ((test_limit.*));");
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_limit WHERE test_limit ==> dsl.limit(-1, dsl.match_all()); ",
        )
        .expect("failed to get SPI result");
        panic!("executed a search with a negative limit.  count={}", count);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_sort_desc() {
        Spi::run("CREATE TABLE test_sort AS SELECT * FROM generate_series(1, 100);");
        Spi::run("CREATE INDEX idxtest_sort ON test_sort USING zombodb ((test_sort.*));");
        Spi::connect(|client| {
            let mut table = client.select("SELECT * FROM test_sort WHERE test_sort ==> dsl.sort('generate_series', 'desc', dsl.match_all());", None, None);

            let mut previous = table.get_one::<i64>().unwrap();
            table.next();
            while let Some(row) = table.next() {
                let col = row.get(0).unwrap().unwrap();
                let current =
                    unsafe { i64::from_datum(col, false, PgBuiltInOids::INT8OID.value()) }.unwrap();

                assert!(current < previous);
                previous = current;
            }
            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_min_score_cutoff() {
        Spi::run("CREATE TABLE test_minscore AS SELECT * FROM generate_series(1, 100);");
        Spi::run(
            "CREATE INDEX idxtest_minscore ON test_minscore USING zombodb ((test_minscore.*));",
        );
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM test_minscore WHERE test_minscore ==> dsl.min_score(2, dsl.match_all()); ",
        )
            .expect("failed to get SPI result");
        assert_eq!(count, 0);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_offset_scan() {
        Spi::run("CREATE TABLE test_offset AS SELECT * FROM generate_series(1, 100);");
        Spi::run("CREATE INDEX idxtest_offset ON test_offset USING zombodb ((test_offset.*));");
        let count = Spi::get_one::<i64>(
            "SELECT * FROM test_offset WHERE test_offset ==> dsl.sort('generate_series', 'asc', dsl.offset(10, dsl.match_all()));"
        )
            .expect("failed to get SPI result");
        assert_eq!(count, 11);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_offset_overflow() {
        Spi::run("CREATE TABLE test_offset AS SELECT * FROM generate_series(1, 100);");
        Spi::run("CREATE INDEX idxtest_offset ON test_offset USING zombodb ((test_offset.*));");
        assert!(Spi::get_one::<i64>(
            "SELECT * FROM test_offset WHERE test_offset ==> dsl.sort('generate_series', 'asc', dsl.offset(1000, dsl.match_all()));"
        ).is_none());
    }
}