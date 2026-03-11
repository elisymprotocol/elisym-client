/// Protocol fee in basis points (300 = 3%). Integer-only arithmetic.
/// Currently hardcoded — will move to on-chain governance in Phase 3.
pub const PROTOCOL_FEE_BPS: u64 = 300;

/// Solana address of the protocol treasury that receives the protocol fee.
/// Currently hardcoded — will move to on-chain governance in Phase 3.
pub const PROTOCOL_TREASURY: &str = "GY7vnWMkKpftU4nQ16C2ATkj1JwrQpHhknkaBUn67VTy";

/// Solana rent-exempt minimum for a 0-data account (lamports).
pub const RENT_EXEMPT_MINIMUM: u64 = 890_880;

/// Validate that the provider's net amount (price minus protocol fee) is above
/// Solana's rent-exempt minimum. Returns an error message if invalid, None if OK.
pub fn validate_job_price(lamports: u64) -> Option<String> {
    if lamports == 0 {
        return None; // free mode
    }
    let fee = (lamports * PROTOCOL_FEE_BPS).div_ceil(10_000);
    let provider_net = lamports.saturating_sub(fee);
    if provider_net < RENT_EXEMPT_MINIMUM {
        Some(format!(
            "Price too low: after {} protocol fee the provider receives {} lamports, \
             which is below Solana rent-exempt minimum ({} lamports).",
            crate::util::format_bps_percent(PROTOCOL_FEE_BPS),
            provider_net,
            RENT_EXEMPT_MINIMUM,
        ))
    } else {
        None
    }
}
