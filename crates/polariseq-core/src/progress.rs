use indicatif::ProgressStyle;

pub fn transfer_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {prefix:<14.bold.cyan} {bar:28.cyan/bright_black} {percent:>3}% {binary_bytes:>9}/{binary_total_bytes:<9} {binary_bytes_per_sec:>10} ETA {eta_precise:>8} {msg:.dim}",
    )
    .expect("valid transfer progress template")
    .progress_chars("█▉▊▋▌▍▎▏░")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

pub fn verify_bar_style() -> ProgressStyle {
    // The prefix column is exactly 26 cells wide (`<26!` pads short names
    // and hard-truncates long ones), so every per-file bar lines up in the
    // same column regardless of file name length. The hash bars in
    // `md5.rs` additionally middle-truncate names so head and tail stay
    // visible within the budget.
    ProgressStyle::with_template(
        "{spinner:.yellow} {prefix:<26!.yellow.dim} {bar:28.green/bright_black} {percent:>3}% {binary_bytes:>9}/{binary_total_bytes:<9} {msg:.dim}",
    )
    .expect("valid verify progress template")
    .progress_chars("█▉▊▋▌▍▎▏░")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

pub fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {prefix:<18.bold.cyan} {msg:.dim}")
        .expect("valid spinner progress template")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

pub fn phase_bar_style() -> ProgressStyle {
    // Keep phase rows compact enough for an 80-column terminal. Unlike the
    // download bar, conversion and compression do not have a reliable ETA.
    ProgressStyle::with_template(
        "{spinner:.cyan} {prefix:<11!.bold.cyan} {bar:14.cyan/bright_black} {percent:>3}% {binary_bytes:>8}/{binary_total_bytes:<8} {wide_msg:.dim}",
    )
    .expect("valid phase progress template")
    .progress_chars("█▉▊▋▌▍▎▏░")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_styles_are_valid() {
        let _ = transfer_bar_style();
        let _ = verify_bar_style();
        let _ = spinner_style();
        let _ = phase_bar_style();
    }
}
