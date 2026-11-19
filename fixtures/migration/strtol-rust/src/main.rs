fn main() {
    let argument = std::env::args().nth(1).expect("one input argument");
    let input = argument.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits = input.strip_prefix('+').or_else(|| input.strip_prefix('-')).unwrap_or(input);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        println!("{{\"error\":\"invalid\"}}");
        return;
    }
    match input.parse::<i32>() {
        Ok(value) => println!("{{\"value\":{value}}}"),
        Err(_) => println!("{{\"error\":\"overflow\"}}"),
    }
}
