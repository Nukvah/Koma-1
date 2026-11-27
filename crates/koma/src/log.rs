//! Structured logs: one JSON object per line on stderr.
//!
//! Deliberately tiny. Timestamps come from the container runtime.

pub fn info(msg: &str, fields: &[(&str, &str)]) {
    eprintln!("{}", line("info", msg, fields));
}

pub fn error(msg: &str, fields: &[(&str, &str)]) {
    eprintln!("{}", line("error", msg, fields));
}

/// Formats one log line, e.g. `{"level":"info","msg":"listening","addr":"[::]:8080"}`.
pub fn line(level: &str, msg: &str, fields: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(64);
    out.push('{');
    push_pair(&mut out, "level", level);
    out.push(',');
    push_pair(&mut out, "msg", msg);
    for (key, value) in fields {
        out.push(',');
        push_pair(&mut out, key, value);
    }
    out.push('}');
    out
}

fn push_pair(out: &mut String, key: &str, value: &str) {
    push_str(out, key);
    out.push(':');
    push_str(out, value);
}

fn push_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::line;

    #[test]
    fn escapes_json() {
        assert_eq!(
            line("info", "say \"hi\"\n", &[("path", "C:\\x\u{1}")]),
            r#"{"level":"info","msg":"say \"hi\"\n","path":"C:\\x\u0001"}"#
        );
    }
}
