//! Pure strategy logic for the price-alert module.
//!
//! Every interaction with the world flows through the [`Host`] trait
//! seam exposed by `shepherd-sdk` — no direct calls to wit-bindgen-
//! generated free functions live here. The `lib.rs` glue wraps a
//! `WitBindgenHost` adapter around the module's per-cdylib wit-bindgen
//! imports and hands it to [`on_block`]; tests under `#[cfg(test)]`
//! hand the same function a `shepherd_sdk_test::MockHost`.

use alloy_primitives::I256;
use alloy_sol_types::{SolCall, sol};
use shepherd_sdk::chain::{eth_call_params, parse_eth_call_result};
use shepherd_sdk::host::{Host, HostError, HostErrorKind, LogLevel};
use shepherd_sdk::prelude::{Address, U256};

sol! {
    /// Chainlink AggregatorV3Interface - only the function this module
    /// needs.
    interface AggregatorV3 {
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
    }
}

/// Resolved configuration, parsed from `module.toml::[config]` at
/// `init` and read on every `on_event`.
#[derive(Debug)]
pub struct Settings {
    /// Chainlink AggregatorV3Interface address.
    pub oracle_address: Address,
    /// Threshold scaled to the oracle's native units
    /// (`threshold_decimal * 10**decimals`).
    pub threshold_scaled: I256,
    /// Which side of the threshold fires.
    pub direction: Direction,
    /// Throttle: only poll every Nth block.
    pub every_n_blocks: u64,
}

/// Which side of the threshold the alert fires on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Fire when `answer >= threshold`.
    Above,
    /// Fire when `answer <= threshold`.
    Below,
}

/// React to a new block.
///
/// Returns `Ok(())` on success and on recoverable upstream failures
/// (oracle RPC error, decode failure) - the strategy logs a Warn and
/// lets the next block re-poll rather than propagating into the
/// supervisor. Only host-level I/O on the persistence side would
/// bubble up via `?`, and this module does not touch the store.
pub fn on_block<H: Host>(
    host: &H,
    chain_id: u64,
    settings: &Settings,
    block_number: u64,
) -> Result<(), HostError> {
    if !block_number.is_multiple_of(settings.every_n_blocks) {
        return Ok(());
    }
    let call_data = AggregatorV3::latestRoundDataCall {}.abi_encode();
    let params = eth_call_params(&settings.oracle_address, &call_data);
    let result_json = match host.request(chain_id, "eth_call", &params) {
        Ok(s) => s,
        Err(err) => {
            host.log(
                LogLevel::Warn,
                &format!(
                    "price-alert eth_call failed ({}): {}",
                    err.code, err.message
                ),
            );
            return Ok(());
        }
    };
    let Some(bytes) = parse_eth_call_result(&result_json) else {
        host.log(
            LogLevel::Warn,
            &format!("price-alert: cannot decode result hex {result_json}"),
        );
        return Ok(());
    };
    let decoded = match AggregatorV3::latestRoundDataCall::abi_decode_returns(&bytes) {
        Ok(d) => d,
        Err(e) => {
            host.log(
                LogLevel::Warn,
                &format!("price-alert: latestRoundData decode failed: {e}"),
            );
            return Ok(());
        }
    };
    let answer = decoded.answer;
    if classify(answer, settings.threshold_scaled, settings.direction) {
        host.log(
            LogLevel::Warn,
            &format!(
                "price-alert: TRIGGERED answer={answer} threshold={} ({:?})",
                settings.threshold_scaled, settings.direction,
            ),
        );
    } else {
        host.log(
            LogLevel::Info,
            &format!(
                "price-alert: ok answer={answer} threshold={} ({:?})",
                settings.threshold_scaled, settings.direction,
            ),
        );
    }
    Ok(())
}

/// `true` when `answer` is on the firing side of `threshold` per
/// `direction`. Pure - exercised by the unit tests.
pub fn classify(answer: I256, threshold: I256, direction: Direction) -> bool {
    match direction {
        Direction::Above => answer >= threshold,
        Direction::Below => answer <= threshold,
    }
}

/// Parse `module.toml::[config]` into a typed [`Settings`].
///
/// One-shot config-parser style: returns `Result<T, HostError>` so the
/// `Guest::init` adapter can lift the failure into the wit-bindgen
/// `HostError` with no extra plumbing.
pub fn parse_config(entries: &[(String, String)]) -> Result<Settings, HostError> {
    let oracle_address = config_get(entries, "oracle_address")?
        .parse::<Address>()
        .map_err(|e| config_err(format!("oracle_address: {e}")))?;
    let decimals = config_get(entries, "decimals")?
        .parse::<u32>()
        .map_err(|e| config_err(format!("decimals: {e}")))?;
    if decimals > 38 {
        return Err(config_err(format!(
            "decimals={decimals} exceeds the I256 power-of-ten budget"
        )));
    }
    let threshold_decimal = config_get(entries, "threshold")?;
    let threshold_scaled = scale_threshold(threshold_decimal, decimals)?;
    let direction = match config_get(entries, "direction")?.to_ascii_lowercase().as_str() {
        "above" => Direction::Above,
        "below" => Direction::Below,
        other => {
            return Err(config_err(format!(
                "direction: expected 'above'|'below', got {other:?}"
            )));
        }
    };
    let every_n_blocks = config_get_optional(entries, "every_n_blocks")
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| config_err(format!("every_n_blocks: {e}")))
        })
        .transpose()?
        .unwrap_or(1)
        .max(1);
    Ok(Settings {
        oracle_address,
        threshold_scaled,
        direction,
        every_n_blocks,
    })
}

fn config_get<'a>(entries: &'a [(String, String)], key: &str) -> Result<&'a str, HostError> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| config_err(format!("missing key {key:?}")))
}

fn config_get_optional<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn config_err(message: impl Into<String>) -> HostError {
    HostError {
        domain: "price-alert".into(),
        kind: HostErrorKind::InvalidInput,
        code: 0,
        message: format!("price-alert: invalid [config]: {}", message.into()),
        data: None,
    }
}

/// Multiply `threshold_decimal` (e.g. `"2500.00"`) by `10**decimals`
/// into an `I256` for direct comparison with the oracle's answer.
fn scale_threshold(threshold_decimal: &str, decimals: u32) -> Result<I256, HostError> {
    let (sign, body) = if let Some(rest) = threshold_decimal.strip_prefix('-') {
        (-1i32, rest)
    } else {
        (1, threshold_decimal)
    };
    let (whole, frac) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None => (body, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return Err(config_err("threshold: empty"));
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(config_err(format!(
            "threshold: non-digit character in {threshold_decimal:?}"
        )));
    }
    let frac_len = frac.len() as u32;
    let composed: String = if frac_len <= decimals {
        let mut s = String::with_capacity(whole.len() + decimals as usize);
        s.push_str(whole);
        s.push_str(frac);
        for _ in 0..(decimals - frac_len) {
            s.push('0');
        }
        s
    } else {
        let mut s = String::with_capacity(whole.len() + decimals as usize);
        s.push_str(whole);
        s.push_str(&frac[..decimals as usize]);
        s
    };
    let raw = if composed.is_empty() { "0" } else { &composed };
    let unsigned: U256 = raw
        .parse()
        .map_err(|e| config_err(format!("threshold parse: {e}")))?;
    let signed = I256::try_from(unsigned)
        .map_err(|e| config_err(format!("threshold range: {e}")))?;
    Ok(if sign < 0 { -signed } else { signed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use shepherd_sdk::host::HostErrorKind as Kind;
    use shepherd_sdk_test::MockHost;

    fn sample_settings(trigger_scaled_dec: i128, direction: Direction) -> Settings {
        Settings {
            oracle_address: "0x694AA1769357215DE4FAC081bf1f309aDC325306".parse().unwrap(),
            threshold_scaled: I256::try_from(trigger_scaled_dec).unwrap(),
            direction,
            every_n_blocks: 1,
        }
    }

    /// Encode a `latestRoundData` return into the `"0x..."` JSON string
    /// the host's `chain::request` would yield.
    fn oracle_response_json(answer_scaled: i128) -> String {
        use alloy_primitives::aliases::U80;
        let returns = AggregatorV3::latestRoundDataReturn {
            roundId: U80::ZERO,
            answer: I256::try_from(answer_scaled).unwrap(),
            startedAt: U256::ZERO,
            updatedAt: U256::ZERO,
            answeredInRound: U80::ZERO,
        };
        let encoded = AggregatorV3::latestRoundDataCall::abi_encode_returns(&returns);
        let hex = hex::encode_prefixed(encoded);
        format!("\"{hex}\"")
    }

    fn programmed_eth_call(host: &MockHost, oracle: Address, response: Result<String, HostError>) {
        let call_data = AggregatorV3::latestRoundDataCall {}.abi_encode();
        let params = eth_call_params(&oracle, &call_data);
        host.chain.respond_to("eth_call", &params, response);
    }

    // ---- pure helpers ----

    #[test]
    fn classify_below_fires_at_or_under_threshold() {
        let t = I256::try_from(100_i32).unwrap();
        assert!(classify(I256::try_from(99_i32).unwrap(), t, Direction::Below));
        assert!(classify(I256::try_from(100_i32).unwrap(), t, Direction::Below));
        assert!(!classify(I256::try_from(101_i32).unwrap(), t, Direction::Below));
    }

    #[test]
    fn classify_above_fires_at_or_over_threshold() {
        let t = I256::try_from(100_i32).unwrap();
        assert!(classify(I256::try_from(101_i32).unwrap(), t, Direction::Above));
        assert!(classify(I256::try_from(100_i32).unwrap(), t, Direction::Above));
        assert!(!classify(I256::try_from(99_i32).unwrap(), t, Direction::Above));
    }

    #[test]
    fn scale_threshold_pads_short_fractional() {
        assert_eq!(
            scale_threshold("1.5", 8).unwrap(),
            I256::try_from(150_000_000_i64).unwrap(),
        );
    }

    #[test]
    fn scale_threshold_truncates_long_fractional() {
        assert_eq!(
            scale_threshold("1.123456789", 8).unwrap(),
            I256::try_from(112_345_678_i64).unwrap(),
        );
    }

    #[test]
    fn scale_threshold_handles_no_decimal_point() {
        assert_eq!(
            scale_threshold("42", 8).unwrap(),
            I256::try_from(4_200_000_000_i64).unwrap(),
        );
    }

    #[test]
    fn scale_threshold_handles_negative_values() {
        assert_eq!(
            scale_threshold("-1.5", 8).unwrap(),
            -I256::try_from(150_000_000_i64).unwrap(),
        );
    }

    #[test]
    fn scale_threshold_rejects_garbage() {
        assert!(matches!(
            scale_threshold("abc", 8).unwrap_err().kind,
            Kind::InvalidInput
        ));
        assert!(matches!(
            scale_threshold("1.2.3", 8).unwrap_err().kind,
            Kind::InvalidInput
        ));
    }

    #[test]
    fn parse_config_happy_path() {
        let entries = vec![
            (
                "oracle_address".into(),
                "0x694AA1769357215DE4FAC081bf1f309aDC325306".into(),
            ),
            ("decimals".into(), "8".into()),
            ("threshold".into(), "2500.50".into()),
            ("direction".into(), "below".into()),
            ("every_n_blocks".into(), "5".into()),
        ];
        let cfg = parse_config(&entries).unwrap();
        assert_eq!(cfg.direction, Direction::Below);
        assert_eq!(cfg.every_n_blocks, 5);
        assert_eq!(
            cfg.threshold_scaled,
            I256::try_from(250_050_000_000_i64).unwrap()
        );
    }

    #[test]
    fn parse_config_defaults_every_n_blocks_to_one() {
        let entries = vec![
            (
                "oracle_address".into(),
                "0x694AA1769357215DE4FAC081bf1f309aDC325306".into(),
            ),
            ("decimals".into(), "8".into()),
            ("threshold".into(), "1".into()),
            ("direction".into(), "above".into()),
        ];
        let cfg = parse_config(&entries).unwrap();
        assert_eq!(cfg.every_n_blocks, 1);
        assert_eq!(cfg.direction, Direction::Above);
    }

    #[test]
    fn parse_config_rejects_missing_key() {
        let entries = vec![
            ("decimals".into(), "8".into()),
            ("threshold".into(), "1".into()),
            ("direction".into(), "above".into()),
        ];
        let err = parse_config(&entries).unwrap_err();
        assert!(matches!(err.kind, Kind::InvalidInput));
        assert!(err.message.contains("oracle_address"));
    }

    // ---- strategy behaviour against MockHost ----

    #[test]
    fn on_block_idle_when_price_above_below_trigger() {
        let host = MockHost::new();
        let settings = sample_settings(/*trigger*/ 250_050_000_000, Direction::Below);
        programmed_eth_call(
            &host,
            settings.oracle_address,
            Ok(oracle_response_json(300_000_000_000)),
        );

        on_block(&host, 11_155_111, &settings, 100).unwrap();

        assert_eq!(host.chain.call_count(), 1);
        assert!(host.logging.contains("ok answer="));
        assert_eq!(host.logging.count_at(LogLevel::Warn), 0);
    }

    #[test]
    fn on_block_triggers_below_threshold() {
        let host = MockHost::new();
        let settings = sample_settings(250_050_000_000, Direction::Below);
        programmed_eth_call(
            &host,
            settings.oracle_address,
            Ok(oracle_response_json(200_000_000_000)),
        );

        on_block(&host, 11_155_111, &settings, 100).unwrap();

        assert!(host.logging.contains("TRIGGERED"));
        assert_eq!(host.logging.count_at(LogLevel::Warn), 1);
    }

    #[test]
    fn on_block_triggers_above_threshold() {
        let host = MockHost::new();
        let settings = sample_settings(100, Direction::Above);
        programmed_eth_call(
            &host,
            settings.oracle_address,
            Ok(oracle_response_json(200)),
        );

        on_block(&host, 11_155_111, &settings, 100).unwrap();

        assert!(host.logging.contains("TRIGGERED"));
    }

    #[test]
    fn on_block_warns_and_continues_on_rpc_error() {
        let host = MockHost::new();
        let settings = sample_settings(100, Direction::Below);
        programmed_eth_call(
            &host,
            settings.oracle_address,
            Err(HostError {
                domain: "chain".into(),
                kind: Kind::Timeout,
                code: 504,
                message: "upstream timed out".into(),
                data: None,
            }),
        );

        // Strategy returns Ok so the supervisor moves on.
        on_block(&host, 11_155_111, &settings, 100).unwrap();
        assert!(host.logging.contains("eth_call failed"));
        // No "TRIGGERED" / "ok answer=" log because we never got an
        // oracle response.
        assert!(!host.logging.contains("TRIGGERED"));
    }

    #[test]
    fn on_block_warns_on_undecodable_result() {
        let host = MockHost::new();
        let settings = sample_settings(100, Direction::Below);
        programmed_eth_call(&host, settings.oracle_address, Ok("not-json".into()));

        on_block(&host, 11_155_111, &settings, 100).unwrap();
        assert!(host.logging.contains("cannot decode result hex"));
    }

    #[test]
    fn on_block_respects_every_n_blocks_throttle() {
        let host = MockHost::new();
        let mut settings = sample_settings(100, Direction::Below);
        settings.every_n_blocks = 5;
        programmed_eth_call(
            &host,
            settings.oracle_address,
            Ok(oracle_response_json(50)),
        );

        // Blocks 1..5 do not poll; only block 5 (which divides evenly).
        for n in 1..5 {
            on_block(&host, 11_155_111, &settings, n).unwrap();
        }
        assert_eq!(host.chain.call_count(), 0);

        on_block(&host, 11_155_111, &settings, 5).unwrap();
        assert_eq!(host.chain.call_count(), 1);
    }
}
