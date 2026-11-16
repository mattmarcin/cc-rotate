//! What a model response would have cost at API prices, as Claude Code estimates it (what
//! `/cost` and the status line's `cost.total_cost_usd` show).
//!
//! The status line reports that estimate only where it runs: in an interactive terminal session.
//! A session the Claude desktop app runs, `claude -p` and SDK clients have none, so the relay
//! prices their responses itself (`relay_proto::status_line_counts_cost` says which), from the
//! `usage` Anthropic sends back and the prices below.
//!
//! The prices are Claude Code's own: its model catalog's pricing tiers (US dollars per million
//! tokens), its fast mode prices, and its default for a model it does not know. The formula is
//! its formula: a long prompt's prices above the tier's threshold, cache writes split into
//! 5-minute and 1-hour ones, ×1.1 for US-only inference, and a flat price per web search.
//! Update both when Claude Code's catalog changes.

use serde::Deserialize;

/// One price list, in US dollars per million tokens (a web search is per request).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub web_search: f64,
    /// Prices for a prompt of more than this many tokens (input, cache reads and cache writes).
    pub long_prompt: Option<(u64, &'static Prices)>,
}

const fn tier(input: f64, output: f64, write_5m: f64, write_1h: f64, read: f64) -> Prices {
    Prices {
        input,
        output,
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
        cache_read: read,
        web_search: 0.01,
        long_prompt: None,
    }
}

const TIER_2_10: Prices = tier(2.0, 10.0, 2.5, 4.0, 0.2);
const TIER_2_10_READ_0_10: Prices = tier(2.0, 10.0, 2.5, 4.0, 0.1);
const TIER_3_15: Prices = tier(3.0, 15.0, 3.75, 6.0, 0.3);
const TIER_5_25: Prices = tier(5.0, 25.0, 6.25, 10.0, 0.5);
const TIER_15_75: Prices = tier(15.0, 75.0, 18.75, 30.0, 1.5);
const TIER_10_50: Prices = tier(10.0, 50.0, 12.5, 20.0, 1.0);
const TIER_10_50_READ_0_25: Prices = tier(10.0, 50.0, 12.5, 20.0, 0.25);
const TIER_4_20_READ_0_20: Prices = tier(4.0, 20.0, 5.0, 8.0, 0.2);
const TIER_8_40_READ_0_20: Prices = tier(8.0, 40.0, 10.0, 16.0, 0.2);
const HAIKU_35: Prices = tier(0.8, 4.0, 1.0, 1.6, 0.08);
const HAIKU_45: Prices = tier(1.0, 5.0, 1.25, 2.0, 0.1);
const HAIKU_55_LONG: Prices = tier(0.5, 2.5, 0.625, 1.0, 0.05);
const HAIKU_55: Prices = Prices {
    long_prompt: Some((100_000, &HAIKU_55_LONG)),
    ..tier(0.1, 0.5, 0.125, 0.2, 0.01)
};

/// Fast mode's prices (`usage.speed` is `"fast"`), for the models that have it.
const FAST_30_150: Prices = tier(30.0, 150.0, 37.5, 60.0, 3.0);
const FAST_10_50: Prices = tier(10.0, 50.0, 12.5, 20.0, 1.0);
const FAST_8_40: Prices = tier(8.0, 40.0, 10.0, 16.0, 0.4);
const FAST_16_80: Prices = tier(16.0, 80.0, 20.0, 32.0, 0.4);

/// What Claude Code charges a model it does not know.
pub const DEFAULT: Prices = TIER_5_25;

/// Claude Code's model catalog: each model's id and its pricing tier.
const CATALOG: &[(&str, &Prices)] = &[
    ("claude-3-5-haiku", &HAIKU_35),
    ("claude-haiku-4-5", &HAIKU_45),
    ("claude-haiku-5-5", &HAIKU_55),
    ("claude-3-5-sonnet", &TIER_3_15),
    ("claude-3-7-sonnet", &TIER_3_15),
    ("claude-sonnet-4-0", &TIER_3_15),
    ("claude-sonnet-4", &TIER_3_15),
    ("claude-sonnet-4-5", &TIER_3_15),
    ("claude-sonnet-4-6", &TIER_3_15),
    ("claude-sonnet-5", &TIER_2_10),
    ("claude-sonnet-5-5", &TIER_2_10_READ_0_10),
    ("claude-opus-4-0", &TIER_15_75),
    ("claude-opus-4", &TIER_15_75),
    ("claude-opus-4-1", &TIER_15_75),
    ("claude-opus-4-5", &TIER_5_25),
    ("claude-opus-4-6", &TIER_5_25),
    ("claude-opus-4-7", &TIER_5_25),
    ("claude-opus-4-8", &TIER_5_25),
    ("claude-opus-5", &TIER_5_25),
    ("claude-opus-5-5", &TIER_4_20_READ_0_20),
    ("claude-fable-5", &TIER_10_50),
    ("claude-fable-5-1", &TIER_10_50_READ_0_25),
    ("claude-fable-5-2", &TIER_8_40_READ_0_20),
    ("claude-mythos-5", &TIER_10_50),
    ("claude-mythos-5-1", &TIER_10_50_READ_0_25),
    ("claude-mythos-5-2", &TIER_8_40_READ_0_20),
];

/// The model's id as the catalog names it: lower case, without a context suffix (`[1m]`) or a
/// trailing release date (`claude-haiku-4-5-20251001`).
fn catalog_id(model: &str) -> String {
    let model = model.trim().to_ascii_lowercase();
    let model = model.split('[').next().unwrap_or("").trim_end();
    match model.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
            head.to_string()
        }
        _ => model.to_string(),
    }
}

/// The prices Claude Code uses for `model`, at the speed the response reports.
pub fn prices(model: Option<&str>, fast: bool) -> &'static Prices {
    let Some(id) = model.map(catalog_id) else {
        return &DEFAULT;
    };
    if fast {
        let fast = match id.as_str() {
            "claude-fable-5-2" | "claude-mythos-5-2" => Some(&FAST_16_80),
            "claude-opus-5-5" => Some(&FAST_8_40),
            "claude-opus-4-8" | "claude-opus-5" => Some(&FAST_10_50),
            "claude-opus-4-6" | "claude-opus-4-7" => Some(&FAST_30_150),
            _ => None,
        };
        if let Some(p) = fast {
            return p;
        }
    }
    CATALOG
        .iter()
        .find(|(name, _)| *name == id)
        .map_or(&DEFAULT, |(_, p)| *p)
}

/// The `usage` of a model response, as far as pricing goes. Every field is optional: a stream
/// reports it in parts (`Usage::merge`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation: Option<CacheCreation>,
    pub server_tool_use: Option<ServerToolUse>,
    /// `"fast"` for fast mode.
    pub speed: Option<String>,
    /// `"us"` for US-only inference, which costs 1.1 times as much.
    pub inference_geo: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CacheCreation {
    pub ephemeral_5m_input_tokens: Option<u64>,
    pub ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerToolUse {
    pub web_search_requests: Option<u64>,
}

impl Usage {
    /// Take in a later report of the same response (`message_delta` after `message_start`), the
    /// way Claude Code does: a token count above zero replaces the earlier one, the output count
    /// and the rest replace it whenever present.
    pub fn merge(&mut self, later: Usage) {
        let positive = |v: Option<u64>| v.filter(|&n| n > 0);
        let later_split = later
            .cache_creation
            .as_ref()
            .map(|c| {
                c.ephemeral_5m_input_tokens.unwrap_or(0) + c.ephemeral_1h_input_tokens.unwrap_or(0)
            })
            .unwrap_or(0);
        self.input_tokens = positive(later.input_tokens).or(self.input_tokens);
        self.cache_creation_input_tokens = positive(later.cache_creation_input_tokens)
            .or(positive(Some(later_split)))
            .or(self.cache_creation_input_tokens);
        self.cache_read_input_tokens =
            positive(later.cache_read_input_tokens).or(self.cache_read_input_tokens);
        self.output_tokens = later.output_tokens.or(self.output_tokens);
        if later_split > 0 {
            let earlier = self.cache_creation.take().unwrap_or_default();
            let c = later.cache_creation.unwrap_or_default();
            self.cache_creation = Some(CacheCreation {
                ephemeral_5m_input_tokens: c
                    .ephemeral_5m_input_tokens
                    .or(earlier.ephemeral_5m_input_tokens),
                ephemeral_1h_input_tokens: c
                    .ephemeral_1h_input_tokens
                    .or(earlier.ephemeral_1h_input_tokens),
            });
        }
        if let Some(later) = later.server_tool_use.and_then(|s| s.web_search_requests) {
            self.server_tool_use = Some(ServerToolUse {
                web_search_requests: Some(later),
            });
        }
        self.speed = later.speed.or(self.speed.take());
        self.inference_geo = later.inference_geo.or(self.inference_geo.take());
    }

    fn fast(&self) -> bool {
        self.speed.as_deref() == Some("fast")
    }
}

/// What a response with this `usage` from `model` would have cost at API prices, in US dollars,
/// as Claude Code works it out.
pub fn cost_usd(model: Option<&str>, usage: &Usage) -> f64 {
    let base = prices(model, usage.fast());
    let input = usage.input_tokens.unwrap_or(0) as f64;
    let output = usage.output_tokens.unwrap_or(0) as f64;
    let read = usage.cache_read_input_tokens.unwrap_or(0) as f64;
    let written = usage.cache_creation_input_tokens.unwrap_or(0) as f64;
    let prompt = input + read + written;
    let p = match base.long_prompt {
        Some((above, long)) if prompt > above as f64 => long,
        _ => base,
    };
    let one_hour = usage
        .cache_creation
        .as_ref()
        .and_then(|c| c.ephemeral_1h_input_tokens)
        .map_or(0.0, |n| (n as f64).clamp(0.0, written));
    let writes = if one_hour > 0.0 {
        one_hour * p.cache_write_1h + (written - one_hour) * p.cache_write_5m
    } else {
        written * p.cache_write_5m
    };
    let tokens = (input * p.input + output * p.output + read * p.cache_read + writes) / 1_000_000.0;
    let geo = if usage.inference_geo.as_deref() == Some("us") {
        1.1
    } else {
        1.0
    };
    let searches = usage
        .server_tool_use
        .as_ref()
        .and_then(|s| s.web_search_requests)
        .unwrap_or(0) as f64;
    tokens * geo + searches * base.web_search
}

/// Reads a model response's body as it streams past and keeps its model and usage: the
/// `message_start` and `message_delta` events of a stream, or the `usage` of a whole JSON
/// response. Everything else passes unread.
#[derive(Debug, Default)]
pub struct UsageTap {
    /// The part of a line not ended yet (a stream), or the whole body so far (JSON).
    pending: Vec<u8>,
    /// `Some(true)` once the body is known to be JSON, `Some(false)` for a stream.
    json: Option<bool>,
    /// Gave up: a line or body too long to keep.
    overflow: bool,
    model: Option<String>,
    usage: Option<Usage>,
}

/// The longest stream line or JSON body the tap keeps. A `message_start` line is a few hundred
/// bytes; a JSON response is the whole message.
const MAX_PENDING: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<MessageHead>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct MessageHead {
    model: Option<String>,
    usage: Option<Usage>,
}

impl UsageTap {
    pub fn new() -> UsageTap {
        UsageTap::default()
    }

    /// Take in the next part of the body.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.overflow || chunk.is_empty() {
            return;
        }
        if self.json.is_none() {
            match chunk.iter().find(|b| !b.is_ascii_whitespace()) {
                Some(b'{') => self.json = Some(true),
                Some(_) => self.json = Some(false),
                None => {}
            }
        }
        if self.json == Some(true) {
            self.keep(chunk);
            return;
        }
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (line, after) = rest.split_at(end);
            if self.pending.is_empty() && !self.overflow {
                self.line(line);
            } else {
                // A line begun in an earlier chunk; one too long to keep is skipped whole.
                self.keep(line);
                let whole = std::mem::take(&mut self.pending);
                if !self.overflow {
                    self.line(&whole);
                }
                self.overflow = false;
            }
            rest = &after[1..];
        }
        self.keep(rest);
    }

    /// Keep `part` for later. Past `MAX_PENDING` the tap gives up on it: on the rest of a stream
    /// line, or on a JSON body for good.
    fn keep(&mut self, part: &[u8]) {
        if self.overflow {
            return;
        }
        if self.pending.len() + part.len() > MAX_PENDING {
            self.pending = Vec::new();
            self.overflow = true;
            return;
        }
        self.pending.extend_from_slice(part);
    }

    /// One line of a stream: only `data:` lines of the two events that carry usage are read.
    fn line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let wanted = |needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
        if !(wanted(b"\"message_start\"") || wanted(b"\"message_delta\"")) {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Event>(data.trim_ascii()) else {
            return;
        };
        match event.kind.as_deref() {
            Some("message_start") => {
                if let Some(message) = event.message {
                    self.model = message.model.or(self.model.take());
                    self.take(message.usage);
                }
            }
            Some("message_delta") => self.take(event.usage),
            _ => {}
        }
    }

    fn take(&mut self, usage: Option<Usage>) {
        let Some(usage) = usage else { return };
        match &mut self.usage {
            Some(seen) => seen.merge(usage),
            None => self.usage = Some(usage),
        }
    }

    /// The body ended (or was cut off): the model it named and its usage, when it reported any.
    pub fn finish(mut self) -> Option<(Option<String>, Usage)> {
        if self.json == Some(true) {
            if self.overflow {
                return None;
            }
            let head: MessageHead = serde_json::from_slice(&self.pending).ok()?;
            return Some((head.model, head.usage?));
        }
        if !self.pending.is_empty() && !self.overflow {
            let last = std::mem::take(&mut self.pending);
            self.line(&last);
        }
        Some((self.model, self.usage?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn catalog_ids_drop_the_date_and_the_context_suffix() {
        assert_eq!(catalog_id("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(catalog_id("claude-opus-5-5[1m]"), "claude-opus-5-5");
        assert_eq!(catalog_id("Claude-Fable-5-2"), "claude-fable-5-2");
        assert_eq!(catalog_id("claude-opus-5-5"), "claude-opus-5-5");
    }

    #[test]
    fn prices_by_model_and_speed() {
        assert_eq!(
            prices(Some("claude-fable-5-2"), false),
            &TIER_8_40_READ_0_20
        );
        assert_eq!(prices(Some("claude-fable-5-2"), true), &FAST_16_80);
        assert_eq!(prices(Some("claude-opus-5-5"), false), &TIER_4_20_READ_0_20);
        assert_eq!(prices(Some("claude-opus-5-5"), true), &FAST_8_40);
        // Fast mode a model does not have is priced as usual.
        assert_eq!(
            prices(Some("claude-sonnet-5-5"), true),
            &TIER_2_10_READ_0_10
        );
        assert_eq!(prices(Some("claude-haiku-4-5-20251001"), false), &HAIKU_45);
        assert_eq!(prices(Some("claude-opus-9"), false), &DEFAULT);
        assert_eq!(prices(None, false), &DEFAULT);
    }

    #[test]
    fn cost_of_a_fable_turn() {
        // The usage of a real Fable response from a transcript.
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":2,"cache_creation_input_tokens":62338,"cache_read_input_tokens":45999,
                "output_tokens":479,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":62338},
                "server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard"}"#,
        )
        .unwrap();
        let want = (2.0 * 8.0 + 479.0 * 40.0 + 45999.0 * 0.2 + 62338.0 * 16.0) / 1e6;
        assert!(close(cost_usd(Some("claude-fable-5-2"), &usage), want));
    }

    #[test]
    fn cost_splits_cache_writes_and_adds_geo_and_searches() {
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            cache_creation_input_tokens: Some(3_000_000),
            cache_read_input_tokens: Some(1_000_000),
            cache_creation: Some(CacheCreation {
                ephemeral_5m_input_tokens: Some(2_000_000),
                ephemeral_1h_input_tokens: Some(1_000_000),
            }),
            server_tool_use: Some(ServerToolUse {
                web_search_requests: Some(3),
            }),
            speed: None,
            inference_geo: Some("us".into()),
        };
        // Opus 5.5: 4 + 20 + 0.2 + 2 × 5 + 1 × 8 = 42.2, × 1.1, plus three searches at a cent.
        assert!(close(
            cost_usd(Some("claude-opus-5-5"), &usage),
            42.2 * 1.1 + 0.03
        ));
    }

    #[test]
    fn a_long_prompt_takes_the_long_prices() {
        let short = Usage {
            input_tokens: Some(100_000),
            output_tokens: Some(1_000),
            ..Usage::default()
        };
        let long = Usage {
            input_tokens: Some(100_001),
            ..short.clone()
        };
        assert!(close(
            cost_usd(Some("claude-haiku-5-5"), &short),
            (100_000.0 * 0.1 + 1_000.0 * 0.5) / 1e6
        ));
        assert!(close(
            cost_usd(Some("claude-haiku-5-5"), &long),
            (100_001.0 * 0.5 + 1_000.0 * 2.5) / 1e6
        ));
    }

    #[test]
    fn delta_usage_merges_as_claude_code_merges_it() {
        let mut usage: Usage = serde_json::from_str(
            r#"{"input_tokens":10,"cache_creation_input_tokens":200,"cache_read_input_tokens":300,"output_tokens":1,
                "cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":0}}"#,
        )
        .unwrap();
        usage.merge(
            serde_json::from_str(
                r#"{"input_tokens":0,"cache_creation_input_tokens":null,"output_tokens":512,
                    "server_tool_use":{"web_search_requests":2}}"#,
            )
            .unwrap(),
        );
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.cache_creation_input_tokens, Some(200));
        assert_eq!(usage.cache_read_input_tokens, Some(300));
        assert_eq!(usage.output_tokens, Some(512));
        assert_eq!(
            usage.server_tool_use.and_then(|s| s.web_search_requests),
            Some(2)
        );
    }

    const STREAM: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-fable-5-2","content":[],"usage":{"input_tokens":5,"cache_creation_input_tokens":1000,"cache_read_input_tokens":2000,"output_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":1000}}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello \"message_delta\" usage"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":300}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    fn stream_usage() -> Usage {
        Usage {
            input_tokens: Some(5),
            output_tokens: Some(300),
            cache_creation_input_tokens: Some(1000),
            cache_read_input_tokens: Some(2000),
            cache_creation: Some(CacheCreation {
                ephemeral_5m_input_tokens: Some(0),
                ephemeral_1h_input_tokens: Some(1000),
            }),
            ..Usage::default()
        }
    }

    #[test]
    fn tap_reads_a_stream_in_any_chunking() {
        for size in [1, 2, 7, 64, 4096] {
            let mut tap = UsageTap::new();
            for chunk in STREAM.as_bytes().chunks(size) {
                tap.feed(chunk);
            }
            let (model, usage) = tap.finish().expect("usage");
            assert_eq!(
                model.as_deref(),
                Some("claude-fable-5-2"),
                "chunks of {size}"
            );
            assert_eq!(usage, stream_usage(), "chunks of {size}");
        }
    }

    #[test]
    fn tap_reads_crlf_lines() {
        let mut tap = UsageTap::new();
        tap.feed(STREAM.replace('\n', "\r\n").as_bytes());
        assert_eq!(tap.finish().unwrap().1, stream_usage());
    }

    #[test]
    fn tap_keeps_what_a_cut_off_stream_reported() {
        let cut = &STREAM[..STREAM.find("event: message_delta").unwrap()];
        let mut tap = UsageTap::new();
        tap.feed(cut.as_bytes());
        let (_, usage) = tap.finish().unwrap();
        assert_eq!(usage.output_tokens, Some(1));
        assert_eq!(usage.input_tokens, Some(5));
    }

    #[test]
    fn tap_reads_a_json_response() {
        let body = r#"{"id":"msg_1","type":"message","model":"claude-opus-5-5","content":[{"type":"text","text":"hi"}],
                       "usage":{"input_tokens":7,"output_tokens":9}}"#;
        let mut tap = UsageTap::new();
        for chunk in body.as_bytes().chunks(5) {
            tap.feed(chunk);
        }
        let (model, usage) = tap.finish().unwrap();
        assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(9));
    }

    #[test]
    fn tap_without_usage_reports_nothing() {
        let mut tap = UsageTap::new();
        tap.feed(br#"{"input_tokens":12}"#);
        assert!(tap.finish().is_none());
        let mut tap = UsageTap::new();
        tap.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert!(tap.finish().is_none());
        assert!(UsageTap::new().finish().is_none());
    }
}
