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
