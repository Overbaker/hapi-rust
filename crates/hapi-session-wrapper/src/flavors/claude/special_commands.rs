#[derive(PartialEq, Debug)]
pub enum SpecialCommand<'a> {
    Compact(&'a str),
    Clear,
    Plan {
        mode: &'static str,
        prompt: Option<&'a str>,
        original: &'a str,
    },
}
pub fn parse(text: &str) -> Option<SpecialCommand<'_>> {
    let s = text.trim();
    if s == "/compact" || s.starts_with("/compact ") {
        return Some(SpecialCommand::Compact(s));
    }
    if s == "/clear" {
        return Some(SpecialCommand::Clear);
    }
    if !s.get(..5)?.eq_ignore_ascii_case("/plan") {
        return None;
    }
    let arg = &s[5..];
    if !arg.is_empty() && !arg.starts_with(char::is_whitespace) {
        return None;
    }
    let arg = arg.trim();
    let off = arg.eq_ignore_ascii_case("off");
    let prompt = (!off && !arg.is_empty()).then(|| {
        for quote in ['"', '\''] {
            if let Some(s) = arg.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
                return s.trim();
            }
        }
        arg
    });
    Some(SpecialCommand::Plan {
        mode: if off { "default" } else { "plan" },
        prompt,
        original: s,
    })
}
#[test]
fn commands() {
    for s in [
        "hello",
        "x /compact",
        "/compact\tx",
        "/clear now",
        "/planner",
        "/PLANx",
    ] {
        assert_eq!(parse(s), None);
    }
    for s in ["/compact", "/compact details"] {
        assert_eq!(parse(s), Some(SpecialCommand::Compact(s)));
    }
    assert_eq!(parse(" /clear "), Some(SpecialCommand::Clear));
    for (s, mode, prompt) in [
        ("/plan", "plan", None),
        ("/PLAN Off", "default", None),
        ("/plan ' text '", "plan", Some("text")),
        ("/plan\nnext", "plan", Some("next")),
    ] {
        assert_eq!(
            parse(s),
            Some(SpecialCommand::Plan {
                mode,
                prompt,
                original: s
            })
        );
    }
}
