//! Request log for the AI Router.
//!
//! One JSON object per line, appended as each request finishes, under
//! `<data root>\logs\ai-requests.jsonl`. The file is pruned to the configured
//! entry count (50 or 100) on every append, so it has a hard ceiling instead of
//! growing until someone notices the disk.
//!
//! Nothing here talks to a model. It records what the router already knows: which
//! model was asked for, where the request really went, how long it took, what it
//! cost, and whether it worked.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One finished request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Unix milliseconds when the request was received.
    pub at: u64,
    /// The endpoint the client called: `chat/completions`, `responses` or
    /// `messages`.
    pub endpoint: String,
    /// The id the client asked for, including the prefix (`risun/gpt-4o`) or the
    /// combo (`combo/fast`).
    pub model: String,
    /// The model id actually sent upstream, without the prefix.
    #[serde(default)]
    pub upstream: String,
    /// Provider name, or empty when the request never reached one.
    #[serde(default)]
    pub provider: String,
    /// HTTP status returned to the client.
    pub status: u16,
    /// Wall-clock duration of the request.
    pub ms: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Estimated cost in USD. Always an estimate: RustBox has no billing
    /// relationship with the provider, so the figure comes from configured or
    /// published per-million rates and is labelled as an estimate everywhere it
    /// is shown.
    #[serde(default)]
    pub cost: f64,
    /// Which API key made the call, by name. Never the secret.
    #[serde(default)]
    pub key: String,
    /// True when the reply was streamed, which is why some entries carry no
    /// token counts: the counts live inside the SSE frames.
    #[serde(default)]
    pub stream: bool,
    /// The failure, when there was one. Short on purpose — the log is a list,
    /// not a stack trace dump.
    #[serde(default)]
    pub error: String,
    /// Time to first token, in milliseconds. Zero when the call did not stream,
    /// because a buffered reply has no first token to time — its whole duration
    /// is the answer. Never presented as zero milliseconds.
    #[serde(default)]
    pub ttft_ms: u64,
    /// Output tokens per second, measured after the first token. Zero when there
    /// is nothing to divide.
    #[serde(default)]
    pub tps: f64,
    /// True when [`Entry::output_tokens`] came from counting stream events rather
    /// than from the provider's own usage report. The page marks those figures
    /// with a tilde: they are close, and they are not the provider's number.
    #[serde(default)]
    pub estimated: bool,
}

impl Entry {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn ok(&self) -> bool {
        self.status < 400
    }
}

/// Serialises every write, so two connections finishing at once cannot interleave
/// a prune with an append and lose an entry.
static WRITE: Mutex<()> = Mutex::new(());

// ---- requests in flight ----------------------------------------------------

/// A request the router is serving right now.
///
/// The log on disk only ever holds finished calls, which is why a long stream
/// used to be invisible until it ended — and the interesting part of a slow
/// answer is exactly the part that has not finished. This is the other half:
/// counters in memory, read by the settings page while the call runs, gone the
/// moment it returns.
///
/// One handle per call, held by the thread serving it and cloned into the
/// registry. `end` is what removes it, and every exit path calls `end`.
pub struct Live {
    /// Stable per call, so the page can tell an update from a new row.
    pub id: u64,
    /// Unix milliseconds when the router accepted the request.
    pub at: u64,
    pub endpoint: String,
    /// The id the client asked for, prefix or `combo/…` included.
    pub model: String,
    pub key: String,
    /// Whether the client asked for a streamed reply.
    pub stream: bool,
    /// Which provider is answering. Overwritten on each attempt, so a fallback
    /// combo that tries three members ends up naming the one that replied.
    provider: Mutex<String>,
    /// Milliseconds from accept to the first byte out of the provider, or
    /// `NO_TOKEN` until it arrives. A sentinel rather than 0, because 0 is a
    /// legal first-token time and "not yet" is not.
    first_token: AtomicU64,
    /// Tokens counted from the event stream so far.
    tokens: AtomicU64,
}

/// Sentinel for "the first token has not arrived".
const NO_TOKEN: u64 = u64::MAX;

/// Everything currently being served, oldest first.
static IN_FLIGHT: Mutex<Vec<std::sync::Arc<Live>>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The request this thread is serving.
    ///
    /// A thread-local rather than a parameter threaded through the call path:
    /// the router serves one request per connection thread and one connection
    /// per request, so "the request this thread is on" is exactly right, and no
    /// signature between here and the forwarding loop has to learn about
    /// logging. [`attach`] sets it, [`detach`] clears it, and the guard in the
    /// router does both around the whole call.
    static CURRENT: std::cell::RefCell<Option<std::sync::Arc<Live>>> =
        const { std::cell::RefCell::new(None) };
}

/// Make `live` the request this thread is serving.
pub fn attach(live: std::sync::Arc<Live>) {
    CURRENT.with(|slot| *slot.borrow_mut() = Some(live));
}

/// Forget the current request. The row itself is removed by [`Live::end`].
pub fn detach() {
    CURRENT.with(|slot| *slot.borrow_mut() = None);
}

/// The request this thread is serving, if any.
pub fn current() -> Option<std::sync::Arc<Live>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Note which provider answered, from anywhere in the call path.
pub fn provider(name: &str) {
    if let Some(live) = current() {
        live.provider(name);
    }
}

/// Note the first byte out of the provider, from anywhere in the call path.
pub fn first_token() {
    if let Some(live) = current() {
        live.first_token();
    }
}

/// Publish the live token count, from anywhere in the call path.
pub fn tokens(count: u64) {
    if let Some(live) = current() {
        live.tokens(count);
    }
}

/// Counts streamed content so the page can show tokens accumulating.
///
/// Deliberately a heuristic. The exact figure is in the last frame's `usage`,
/// which by definition has not arrived yet, so a live count has to come from the
/// deltas — and one delta per token is what every provider here emits.
#[derive(Default)]
pub struct TokenCounter {
    buf: String,
    scanned: usize,
    tokens: u64,
}

impl TokenCounter {
    /// Field names that carry a text delta, one per protocol: `content` for chat
    /// completions, `text` for Anthropic messages, `delta` for the Responses
    /// API. Each is matched only when followed by a quoted string, so an object
    /// under the same key (`"delta":{`) is not mistaken for content.
    const NEEDLES: [&'static str; 3] = ["\"content\":\"", "\"delta\":\"", "\"text\":\""];

    pub fn feed(&mut self, text: &str) {
        self.buf.push_str(text);
        for needle in Self::NEEDLES {
            loop {
                let Some(rel) = self.buf[self.scanned..].find(needle) else {
                    break;
                };
                let quote_at = self.scanned + rel + needle.len();
                match self.buf[quote_at..].chars().next() {
                    // The chunk boundary fell inside the needle. Leave the
                    // cursor where it is and wait for the rest rather than
                    // skipping a token that has not arrived yet.
                    None => {
                        self.scanned += rel;
                        break;
                    }
                    Some(c) => {
                        if c != '"' && c != '\\' {
                            self.tokens += 1;
                        }
                        self.scanned = quote_at;
                    }
                }
            }
        }
        // Keep the buffer small without ever re-reading a needle: drop the
        // scanned prefix and move the cursor back by the same amount.
        if self.buf.len() > 8192 {
            let cut = self.buf.len() - 128;
            self.buf.drain(..cut);
            self.scanned = self.scanned.saturating_sub(cut);
        }
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

impl Live {
    /// Which provider is answering.
    pub fn provider(&self, name: &str) {
        if let Ok(mut slot) = self.provider.lock() {
            *slot = name.to_string();
        }
    }

    /// The provider that answered, empty while none has.
    pub fn provider_name(&self) -> String {
        match self.provider.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The first byte out of the provider. Only the first call has any effect,
    /// so the forwarding loop calls it on every chunk without a flag of its own.
    pub fn first_token(&self) {
        let elapsed = now_ms().saturating_sub(self.at);
        let _ = self
            .first_token
            .compare_exchange(NO_TOKEN, elapsed, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// How long the first token took, if it has arrived.
    pub fn ttft(&self) -> Option<u64> {
        match self.first_token.load(Ordering::SeqCst) {
            NO_TOKEN => None,
            ms => Some(ms),
        }
    }

    /// Replace the live token count with a fresh reading from the counter.
    pub fn tokens(&self, tokens: u64) {
        self.tokens.store(tokens, Ordering::SeqCst);
    }

    /// Remove this request from the registry. Idempotent, so a caller can end in
    /// both the success and the error path without tracking which ran.
    pub fn end(&self) {
        if let Ok(mut list) = IN_FLIGHT.lock() {
            list.retain(|row| row.id != self.id);
        }
    }
}

/// Register a request that is about to be served.
pub fn begin(
    endpoint: &str,
    model: &str,
    key: Option<&str>,
    stream: bool,
) -> std::sync::Arc<Live> {
    let live = std::sync::Arc::new(Live {
        id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
        at: now_ms(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        key: key.unwrap_or("—").to_string(),
        stream,
        provider: Mutex::new(String::new()),
        first_token: AtomicU64::new(NO_TOKEN),
        tokens: AtomicU64::new(0),
    });
    if let Ok(mut list) = IN_FLIGHT.lock() {
        // A leaked row would sit in the list forever. The cap is generous
        // enough for real concurrency and small enough that a leak cannot grow
        // the list without bound; the oldest rows are the likeliest leaks.
        if list.len() >= 64 {
            let cut = list.len() - 63;
            list.drain(0..cut);
        }
        list.push(live.clone());
    }
    live
}

/// A snapshot of one in-flight request, in the shape the page renders.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveRow {
    pub id: u64,
    pub at: u64,
    pub endpoint: String,
    pub model: String,
    pub provider: String,
    pub key: String,
    pub stream: bool,
    /// Milliseconds since the request was accepted.
    pub elapsed_ms: u64,
    /// Time to first token, absent until the first byte arrives.
    pub ttft_ms: Option<u64>,
    /// Tokens counted so far. Always an estimate: the provider's own figure
    /// only arrives with the last frame.
    pub output_tokens: u64,
    /// Tokens per second over the generation time so far, absent until there is
    /// something to divide.
    pub tps: Option<f64>,
}

/// Everything in flight, newest first — the order the log list uses.
pub fn in_flight() -> Vec<LiveRow> {
    let rows = match IN_FLIGHT.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let now = now_ms();
    let mut out: Vec<LiveRow> = rows
        .iter()
        .map(|live| {
            // Elapsed is recomputed here, not stored: a model that thinks for
            // eight seconds before its first token emits nothing during that
            // time, and the display has to keep moving anyway.
            let elapsed_ms = now.saturating_sub(live.at);
            let ttft_ms = live.ttft();
            let output_tokens = live.tokens.load(Ordering::SeqCst);
            let tps = generation_tps(output_tokens, elapsed_ms, ttft_ms.unwrap_or(0));
            LiveRow {
                id: live.id,
                at: live.at,
                endpoint: live.endpoint.clone(),
                model: live.model.clone(),
                provider: live.provider_name(),
                key: live.key.clone(),
                stream: live.stream,
                elapsed_ms,
                ttft_ms,
                output_tokens,
                tps,
            }
        })
        .collect();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// Output tokens per second, measured after the first token.
///
/// `ttft_ms` is subtracted because the wait for the first token is latency, not
/// throughput: a model that thinks for five seconds and then writes 100 tokens
/// in one second is a 100 t/s model, not a 20 t/s one. `None` when there is
/// nothing to divide, so the page can show a dash instead of a fabricated zero.
pub fn generation_tps(output_tokens: u64, elapsed_ms: u64, ttft_ms: u64) -> Option<f64> {
    let generating = elapsed_ms.saturating_sub(ttft_ms);
    if output_tokens == 0 || generating == 0 {
        return None;
    }
    Some((output_tokens as f64 * 1000.0 / generating as f64 * 10.0).round() / 10.0)
}


/// Directory holding router logs.
pub fn dir() -> PathBuf {
    crate::paths::root().join("logs")
}

pub fn file() -> PathBuf {
    dir().join("ai-requests.jsonl")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Append one entry and prune the file to `limit` lines.
///
/// The prune is a rewrite of the whole file, which is cheap at 50–100 entries and
/// keeps the store a single readable file rather than a directory of shards.
/// Failures are swallowed: a log write must never turn a successful model call
/// into an error the client sees.
pub fn record(entry: &Entry, limit: usize) {
    let limit = limit.clamp(1, 1000);
    // The aggregates are written first and separately: "Clear log" deletes the
    // file below, and the dashboard has to survive that. Doing it here, on the
    // one path every finished call goes through, is what keeps the two in step.
    crate::ai_stats::record(entry);
    let _guard = match WRITE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = file();
    let line = match serde_json::to_string(entry) {
        Ok(l) => l,
        Err(_) => return,
    };
    // Read, append, prune — one pass. Reading first also recovers from a file
    // that a previous crash left without a trailing newline.
    let mut lines: Vec<String> = read_lines(&path);
    lines.push(line);
    if lines.len() > limit {
        let cut = lines.len() - limit;
        lines.drain(0..cut);
    }
    let mut text = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    // Write-then-rename so a half-written file is never left behind: the log is
    // read by the settings page while the router keeps appending to it.
    let temp = path.with_extension("jsonl.tmp");
    if std::fs::write(&temp, text).is_err() {
        return;
    }
    let _ = std::fs::rename(&temp, &path);
}

fn read_lines(path: &std::path::Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Every recorded entry, oldest first.
///
/// A line that does not parse is skipped rather than failing the read: one
/// truncated write must not make the whole history unreadable.
pub fn all() -> Vec<Entry> {
    read_lines(&file())
        .iter()
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .collect()
}

/// Delete the log file.
pub fn clear() -> Result<(), String> {
    let _guard = match WRITE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let path = file();
    if !path.exists() {
        return Ok(());
    }
    std::fs::remove_file(&path).map_err(|e| format!("could not delete {}: {e}", path.display()))
}

/// Bytes the log currently occupies, so the page can say what pruning saves.
pub fn size() -> u64 {
    std::fs::metadata(file()).map(|m| m.len()).unwrap_or(0)
}

/// Append a line to the router's own diagnostic log.
///
/// Separate from the request log on purpose: this one is for the router's own
/// lifecycle (a port that failed to bind, a tunnel that would not start), which a
/// user needs when the request log is empty precisely because nothing works.
pub fn note(message: &str) {
    use std::io::Write as _;
    let dir = dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ai-router.log"))
    {
        let _ = writeln!(file, "[{}] {message}", now_ms());
    }
}

/// Aggregated view for the dashboard.
#[derive(Debug, Default, serde::Serialize)]
pub struct Stats {
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    pub avg_ms: u64,
    /// Mean time to first token across the calls that reported one. Zero when
    /// none did, which the page shows as a dash rather than as instant.
    pub avg_ttft_ms: u64,
    /// Output tokens per second over the window, generation time only.
    pub avg_tps: f64,
    /// How many hours the totals cover, so the page can say "last 24 hours"
    /// from the data instead of from a constant it has to keep in step.
    pub window_hours: u64,
    /// Per-model totals, highest token count first.
    pub models: Vec<ModelStat>,
    /// Per-hour buckets for the chart, oldest first. Always `window_hours`
    /// entries so the chart has a fixed width whether the log holds two requests
    /// or two hundred.
    pub series: Vec<Bucket>,
    /// Calls per endpoint, so the page can say what clients actually use.
    pub endpoints: Vec<ModelStat>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct ModelStat {
    pub name: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Bucket {
    /// Hours before now, 23 = oldest.
    pub hours_ago: u64,
    pub requests: u64,
    pub tokens: u64,
}

/// Aggregate `entries` into the numbers the dashboard shows.
///
/// `now` is passed in so the test can pin the clock instead of racing it.
pub fn summarize(entries: &[Entry], now: u64) -> Stats {
    use std::collections::HashMap;
    let mut stats = Stats::default();
    let mut models: HashMap<String, ModelStat> = HashMap::new();
    let mut endpoints: HashMap<String, ModelStat> = HashMap::new();
    let mut buckets: Vec<Bucket> = (0..24)
        .rev()
        .map(|hours_ago| Bucket {
            hours_ago,
            requests: 0,
            tokens: 0,
        })
        .collect();
    let hour = 3_600_000u64;

    let mut ms_total = 0u64;
    let mut ttft_total = 0u64;
    let mut ttft_count = 0u64;
    for entry in entries {
        stats.requests += 1;
        if !entry.ok() {
            stats.failed += 1;
        }
        stats.input_tokens += entry.input_tokens;
        stats.output_tokens += entry.output_tokens;
        stats.cost += entry.cost;
        ms_total += entry.ms;
        if entry.ttft_ms > 0 {
            ttft_total += entry.ttft_ms;
            ttft_count += 1;
        }

        let model = models.entry(entry.model.clone()).or_insert_with(|| ModelStat {
            name: entry.model.clone(),
            ..ModelStat::default()
        });
        model.requests += 1;
        model.input_tokens += entry.input_tokens;
        model.output_tokens += entry.output_tokens;
        model.cost += entry.cost;

        let endpoint = endpoints
            .entry(entry.endpoint.clone())
            .or_insert_with(|| ModelStat {
                name: entry.endpoint.clone(),
                ..ModelStat::default()
            });
        endpoint.requests += 1;
        endpoint.input_tokens += entry.input_tokens;
        endpoint.output_tokens += entry.output_tokens;
        endpoint.cost += entry.cost;

        if entry.at <= now {
            let ago = (now - entry.at) / hour;
            if ago < 24 {
                let slot = 23 - ago as usize;
                buckets[slot].requests += 1;
                buckets[slot].tokens += entry.total_tokens();
            }
        }
    }

    if stats.requests > 0 {
        stats.avg_ms = ms_total / stats.requests;
    }
    if ttft_count > 0 {
        stats.avg_ttft_ms = ttft_total / ttft_count;
    }
    if stats.output_tokens > 0 && ms_total > ttft_total {
        stats.avg_tps = crate::ai_stats::round1(
            stats.output_tokens as f64 / ((ms_total - ttft_total) as f64 / 1000.0),
        );
    }
    stats.window_hours = 24;
    stats.models = models.into_values().collect();
    stats
        .models
        .sort_by(|a, b| b.total_tokens().cmp(&a.total_tokens()).then(a.name.cmp(&b.name)));
    stats.endpoints = endpoints.into_values().collect();
    stats
        .endpoints
        .sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
    stats.series = buckets;
    stats
}

impl ModelStat {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: u64, model: &str, input: u64, output: u64, status: u16) -> Entry {
        Entry {
            at,
            endpoint: "chat/completions".to_string(),
            model: model.to_string(),
            upstream: model.to_string(),
            provider: "test".to_string(),
            status,
            ms: 100,
            input_tokens: input,
            output_tokens: output,
            cost: 0.0,
            key: "Key".to_string(),
            stream: false,
            error: String::new(),
            ttft_ms: 0,
            tps: 0.0,
            estimated: false,
        }
    }

    #[test]
    fn the_summary_counts_failures_and_ranks_models_by_tokens() {
        let now = 1_000 * 3_600_000u64;
        let entries = vec![
            entry(now - 1000, "a/one", 100, 50, 200),
            entry(now - 2000, "a/one", 10, 5, 200),
            entry(now - 3000, "b/two", 400, 100, 500),
        ];
        let stats = summarize(&entries, now);
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.input_tokens, 510);
        assert_eq!(stats.output_tokens, 155);
        // `b/two` spent 500 tokens against `a/one`'s 165, so it leads.
        assert_eq!(stats.models[0].name, "b/two");
        assert_eq!(stats.models[1].name, "a/one");
        assert_eq!(stats.series.len(), 24);
        assert_eq!(
            stats.series.iter().map(|b| b.requests).sum::<u64>(),
            3,
            "every request lands in a bucket"
        );
    }

    #[test]
    fn an_entry_older_than_the_chart_still_counts_but_is_not_bucketed() {
        let now = 1_000 * 3_600_000u64;
        let entries = vec![entry(now - 30 * 3_600_000, "a/one", 10, 10, 200)];
        let stats = summarize(&entries, now);
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.series.iter().map(|b| b.requests).sum::<u64>(), 0);
    }

    #[test]
    fn an_empty_log_summarizes_to_zero_without_panicking() {
        let stats = summarize(&[], 0);
        assert_eq!(stats.requests, 0);
        assert_eq!(stats.avg_ms, 0);
        assert!(stats.models.is_empty());
        assert_eq!(stats.series.len(), 24);
    }

    #[test]
    fn the_log_dir_lives_under_the_data_root() {
        assert!(dir().starts_with(crate::paths::root()));
        assert_eq!(file(), dir().join("ai-requests.jsonl"));
    }

    #[test]
    fn a_streamed_delta_is_counted_once_per_token_not_once_per_chunk() {
        let mut counter = TokenCounter::default();
        // Two frames, one token each: the count is 2, not 1.
        counter.feed("data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n");
        assert_eq!(counter.tokens(), 1);
        counter.feed("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n");
        assert_eq!(counter.tokens(), 2);
    }

    #[test]
    fn an_empty_delta_is_not_a_token() {
        let mut counter = TokenCounter::default();
        // The final frame carries `content:""` to signal the end of the answer.
        // Counting it would make every stream one token too long.
        counter.feed("data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\n");
        assert_eq!(counter.tokens(), 0);
    }

    #[test]
    fn a_key_that_is_not_a_string_is_not_a_token() {
        let mut counter = TokenCounter::default();
        // Anthropic opens with `"delta":{"type":"message_start",…}`; the object
        // is not content, and counting it would invent a token.
        counter.feed("event: message_start\ndata: {\"delta\":{\"type\":\"message_start\"}}\n\n");
        assert_eq!(counter.tokens(), 0);
    }

    #[test]
    fn a_needle_split_across_two_chunks_is_still_counted() {
        let mut counter = TokenCounter::default();
        // The read boundary can fall anywhere, including inside `"content":"`.
        counter.feed("data: {\"choices\":[{\"delta\":{\"cont");
        counter.feed("ent\":\"hi\"}}]}\n\n");
        assert_eq!(counter.tokens(), 1);
    }

    #[test]
    fn the_counter_buffer_stays_bounded_on_a_long_stream() {
        let mut counter = TokenCounter::default();
        for _ in 0..2_000 {
            counter.feed("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
        }
        assert_eq!(counter.tokens(), 2_000);
        assert!(counter.buf.len() <= 8192, "the window is trimmed as it grows");
    }

    #[test]
    fn throughput_excludes_the_wait_for_the_first_token() {
        // 100 tokens written in 1000 ms after a 5000 ms first token is 100 t/s,
        // not 16.7: the five seconds were latency, not generation.
        assert_eq!(generation_tps(100, 6_000, 5_000), Some(100.0));
        assert_eq!(generation_tps(100, 6_000, 0), Some(16.7));
    }

    #[test]
    fn throughput_is_absent_rather_than_zero_when_there_is_nothing_to_divide() {
        assert_eq!(generation_tps(0, 5_000, 100), None);
        assert_eq!(generation_tps(10, 100, 100), None, "no generation time");
        assert_eq!(generation_tps(10, 50, 100), None, "ttft past the end");
    }

    #[test]
    fn a_finished_request_leaves_the_in_flight_list() {
        let live = begin("chat/completions", "a/one", Some("Key"), true);
        assert!(
            in_flight().iter().any(|row| row.id == live.id),
            "a started request is visible while it runs"
        );
        live.end();
        assert!(
            !in_flight().iter().any(|row| row.id == live.id),
            "and gone once it ends"
        );
        // Ending twice must not panic or remove somebody else's row.
        live.end();
    }

    #[test]
    fn the_first_token_time_is_taken_once_and_kept() {
        let live = begin("chat/completions", "a/one", None, true);
        assert_eq!(live.ttft(), None, "nothing has arrived yet");
        live.first_token();
        let first = live.ttft().expect("a time was recorded");
        live.first_token();
        assert_eq!(live.ttft(), Some(first), "a later chunk does not overwrite it");
        live.end();
    }

    #[test]
    fn an_entry_written_before_these_fields_existed_still_parses() {
        // The log is append-only across versions, so an old line without `ttft_ms`
        // or `tps` must not make the whole history unreadable.
        let old = r#"{"at":1700000000000,"endpoint":"chat/completions","model":"a/one",
            "status":200,"ms":120,"input_tokens":3,"output_tokens":4}"#;
        let entry: Entry = serde_json::from_str(old).expect("old line parses");
        assert_eq!(entry.ttft_ms, 0);
        assert_eq!(entry.tps, 0.0);
        assert!(!entry.estimated);
    }
}
