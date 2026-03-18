use elisym_core::SolanaNetwork;

/// Format lamports as SOL with full 9-decimal precision (integer-only arithmetic).
pub fn format_sol(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = lamports % 1_000_000_000;
    format!("{}.{:09}", whole, frac)
}

/// Format lamports as SOL with 4-decimal compact display (integer-only arithmetic).
pub fn format_sol_compact(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = (lamports % 1_000_000_000) / 100_000;
    format!("{}.{:04}", whole, frac)
}

/// Parse a SOL amount string (e.g. "1.5") into lamports using integer-only arithmetic.
/// Returns None for invalid input, zero/negative amounts, or > 9 decimal places.
pub fn sol_to_lamports(sol_str: &str) -> Option<u64> {
    let s = sol_str.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.splitn(2, '.').collect();
    let whole: u64 = parts[0].parse().ok()?;
    let frac: u64 = if parts.len() == 2 {
        let frac_str = parts[1];
        if frac_str.is_empty() || frac_str.len() > 9 {
            return None;
        }
        let padded = format!("{:0<9}", frac_str);
        padded.parse().ok()?
    } else {
        0
    };
    whole.checked_mul(1_000_000_000)?.checked_add(frac)
}

/// Parse a network name string into the elisym-core SolanaNetwork enum.
pub fn parse_network(s: &str) -> SolanaNetwork {
    match s {
        "mainnet" => SolanaNetwork::Mainnet,
        "testnet" => SolanaNetwork::Testnet,
        "devnet" => SolanaNetwork::Devnet,
        other => SolanaNetwork::Custom(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sol_zero() {
        assert_eq!(format_sol(0), "0.000000000");
    }

    #[test]
    fn format_sol_one_lamport() {
        assert_eq!(format_sol(1), "0.000000001");
    }

    #[test]
    fn format_sol_one_sol() {
        assert_eq!(format_sol(1_000_000_000), "1.000000000");
    }

    #[test]
    fn format_sol_fractional() {
        assert_eq!(format_sol(1_500_000_000), "1.500000000");
    }

    #[test]
    fn format_sol_compact_zero() {
        assert_eq!(format_sol_compact(0), "0.0000");
    }

    #[test]
    fn format_sol_compact_one_sol() {
        assert_eq!(format_sol_compact(1_000_000_000), "1.0000");
    }

    #[test]
    fn format_sol_compact_fractional_truncation() {
        // 1_500_000_000 → frac = 500_000_000 / 100_000 = 5000
        assert_eq!(format_sol_compact(1_500_000_000), "1.5000");
        // 1_123_456_789 → frac = 123_456_789 / 100_000 = 1234
        assert_eq!(format_sol_compact(1_123_456_789), "1.1234");
    }

    #[test]
    fn sol_to_lamports_whole_with_decimal() {
        assert_eq!(sol_to_lamports("1.0"), Some(1_000_000_000));
    }

    #[test]
    fn sol_to_lamports_smallest_unit() {
        assert_eq!(sol_to_lamports("0.000000001"), Some(1));
    }

    #[test]
    fn sol_to_lamports_whole_no_decimal() {
        assert_eq!(sol_to_lamports("1"), Some(1_000_000_000));
    }

    #[test]
    fn sol_to_lamports_half() {
        assert_eq!(sol_to_lamports("0.5"), Some(500_000_000));
    }

    #[test]
    fn sol_to_lamports_invalid_inputs() {
        assert_eq!(sol_to_lamports(""), None);
        assert_eq!(sol_to_lamports("  "), None);
        assert_eq!(sol_to_lamports("1."), None);
        assert_eq!(sol_to_lamports(".5"), None);
        assert_eq!(sol_to_lamports("0.1234567890"), None); // >9 decimals
    }

    #[test]
    fn parse_network_known() {
        assert_eq!(parse_network("mainnet"), SolanaNetwork::Mainnet);
        assert_eq!(parse_network("devnet"), SolanaNetwork::Devnet);
        assert_eq!(parse_network("testnet"), SolanaNetwork::Testnet);
    }

    #[test]
    fn parse_network_custom() {
        assert_eq!(
            parse_network("https://my-rpc.example.com"),
            SolanaNetwork::Custom("https://my-rpc.example.com".to_string()),
        );
    }
}
