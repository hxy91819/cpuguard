pub fn fit_col(input: &str, width: usize) -> String {
    let len = input.chars().count();
    if len == width {
        return input.to_string();
    }
    if len < width {
        return format!("{input:<width$}");
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut out = String::with_capacity(width);
    for ch in input.chars().take(width - 3) {
        out.push(ch);
    }
    out.push_str("...");
    out
}
