use std::borrow::Cow;

const MAX_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 16;

pub(super) fn present(source: &str) -> Cow<'_, str> {
    if source
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        let safe: String = source
            .chars()
            .map(|c| {
                if c.is_control() && c != '\n' && c != '\t' {
                    '�'
                } else {
                    c
                }
            })
            .collect();
        return Cow::Owned(safe);
    }
    if source.len() > MAX_BYTES {
        return Cow::Borrowed(source);
    }
    let mut parser = Math { source, pos: 0 };
    match parser.sequence(0, false) {
        Some((output, _)) if parser.pos == source.len() => Cow::Owned(output),
        _ => Cow::Borrowed(source),
    }
}

struct Math<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> Math<'a> {
    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn take(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn group(&mut self, depth: usize) -> Option<(String, bool)> {
        if self.take()? != '{' || depth > MAX_DEPTH {
            return None;
        }
        self.sequence(depth, true)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.take();
        }
    }

    fn argument(&mut self, depth: usize) -> Option<(String, bool)> {
        self.skip_space();
        self.group(depth)
    }

    fn text_argument(&mut self, depth: usize, roman: bool) -> Option<Cow<'a, str>> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_space();
        if self.take()? != '{' {
            return None;
        }
        let start = self.pos;
        let mut segment = start;
        let mut output: Option<String> = None;
        loop {
            let at = self.pos;
            match self.take()? {
                '}' => {
                    let rest = &self.source[segment..at];
                    return Some(if let Some(mut output) = output {
                        output.push_str(rest);
                        Cow::Owned(output)
                    } else {
                        Cow::Borrowed(&self.source[start..at])
                    });
                }
                '\\' if !roman => {
                    let escaped = self.take()?;
                    if !matches!(escaped, '_' | '%' | '#' | '$' | '&' | '{' | '}') {
                        return None;
                    }
                    let output = output.get_or_insert_with(String::new);
                    output.push_str(&self.source[segment..at]);
                    output.push(escaped);
                    segment = self.pos;
                }
                c if roman && !(c.is_alphanumeric() || c.is_whitespace()) => return None,
                '{' | '\\' | '_' | '^' | '$' | '%' | '#' | '&' => return None,
                _ => {}
            }
        }
    }

    fn sequence(&mut self, depth: usize, group: bool) -> Option<(String, bool)> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut out = String::new();
        let mut units = 0usize;
        let mut simple = true;
        let mut base = false;
        let mut superscript = false;
        let mut subscript = false;
        while let Some(c) = self.peek() {
            match c {
                '}' if group => {
                    self.take();
                    return Some((out, simple && units == 1));
                }
                '}' => return None,
                '{' => {
                    let (body, _) = self.group(depth + 1)?;
                    units += 1;
                    out.push('(');
                    out.push_str(&body);
                    out.push(')');
                    base = true;
                    superscript = false;
                    subscript = false;
                }
                '^' | '_' => {
                    if !base || (c == '^' && superscript) || (c == '_' && subscript) {
                        return None;
                    }
                    superscript |= c == '^';
                    subscript |= c == '_';
                    self.take();
                    let body = if self.peek() == Some('{') {
                        Cow::Owned(self.group(depth + 1)?.0)
                    } else {
                        if self
                            .peek()
                            .is_none_or(|c| c.is_whitespace() || "+-=*/".contains(c))
                        {
                            return None;
                        }
                        self.atom(depth + 1)?
                    };
                    if body.is_empty() {
                        return None;
                    }
                    out.push(c);
                    out.push('(');
                    out.push_str(&body);
                    out.push(')');
                }
                _ => {
                    let start = self.pos;
                    let fraction = self.source[start..].starts_with("\\frac")
                        && self.source[start + 5..]
                            .chars()
                            .next()
                            .is_none_or(|c| !c.is_ascii_alphabetic());
                    let atom = self.atom(depth + 1)?;
                    let source_atom = &self.source[start..self.pos];
                    let adjacent = fraction && {
                        let previous = self.source[..start]
                            .chars()
                            .rev()
                            .find(|c| !c.is_whitespace());
                        let next = self.source[self.pos..].chars().find(|c| !c.is_whitespace());
                        previous.is_some_and(|c| !"+-=([{,<>".contains(c))
                            || next.is_some_and(|c| !"+-=)]},<>".contains(c))
                    };
                    if fraction && adjacent {
                        out.push('(');
                        out.push_str(&atom);
                        out.push(')');
                    } else {
                        out.push_str(&atom);
                    }
                    if !c.is_whitespace() {
                        units += 1;
                        simple &= if c == '\\' {
                            source_atom.starts_with("\\sqrt{")
                                || source_atom.starts_with("\\sqrt ")
                                || source_atom.starts_with("\\mathbb{")
                                || source_atom.starts_with("\\mathbb ")
                                || matches!(
                                    source_atom,
                                    "\\alpha"
                                        | "\\beta"
                                        | "\\gamma"
                                        | "\\delta"
                                        | "\\epsilon"
                                        | "\\theta"
                                        | "\\lambda"
                                        | "\\mu"
                                        | "\\pi"
                                        | "\\rho"
                                        | "\\sigma"
                                        | "\\tau"
                                        | "\\phi"
                                        | "\\omega"
                                        | "\\Gamma"
                                        | "\\Delta"
                                        | "\\Theta"
                                        | "\\Lambda"
                                        | "\\Pi"
                                        | "\\Sigma"
                                        | "\\Phi"
                                        | "\\Omega"
                                        | "\\infty"
                                        | "\\emptyset"
                                )
                        } else {
                            c.is_alphanumeric()
                        };
                        base = true;
                        superscript = false;
                        subscript = false;
                    }
                }
            }
        }
        if group {
            None
        } else {
            Some((out, simple && units == 1))
        }
    }

    fn atom(&mut self, depth: usize) -> Option<Cow<'a, str>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let start = self.pos;
        let c = self.take()?;
        if c != '\\' {
            return if matches!(c, '{' | '}' | '^' | '_') {
                None
            } else {
                Some(Cow::Borrowed(&self.source[start..self.pos]))
            };
        }
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.take();
        }
        if start == self.pos {
            return None;
        }
        let command = &self.source[start..self.pos];
        let symbol = match command {
            "log" => "log",
            "div" => "÷",
            "times" => "×",
            "cdot" => "·",
            "pm" => "±",
            "mp" => "∓",
            "le" | "leq" => "≤",
            "ge" | "geq" => "≥",
            "ne" | "neq" => "≠",
            "approx" => "≈",
            "equiv" => "≡",
            "in" => "∈",
            "notin" => "∉",
            "subset" => "⊂",
            "subseteq" => "⊆",
            "cup" => "∪",
            "cap" => "∩",
            "emptyset" => "∅",
            "to" | "rightarrow" => "→",
            "leftarrow" => "←",
            "leftrightarrow" => "↔",
            "Rightarrow" => "⇒",
            "infty" => "∞",
            "sum" => "∑",
            "prod" => "∏",
            "int" => "∫",
            "alpha" => "α",
            "beta" => "β",
            "gamma" => "γ",
            "delta" => "δ",
            "epsilon" => "ε",
            "theta" => "θ",
            "lambda" => "λ",
            "mu" => "μ",
            "pi" => "π",
            "rho" => "ρ",
            "sigma" => "σ",
            "tau" => "τ",
            "phi" => "φ",
            "omega" => "ω",
            "Gamma" => "Γ",
            "Delta" => "Δ",
            "Theta" => "Θ",
            "Lambda" => "Λ",
            "Pi" => "Π",
            "Sigma" => "Σ",
            "Phi" => "Φ",
            "Omega" => "Ω",
            "frac" => {
                let (numerator, numerator_simple) = self.argument(depth + 1)?;
                let (denominator, denominator_simple) = self.argument(depth + 1)?;
                if numerator.is_empty() || denominator.is_empty() {
                    return None;
                }
                let mut fraction = String::with_capacity(
                    numerator.len()
                        + denominator.len()
                        + 1
                        + if numerator_simple { 0 } else { 2 }
                        + if denominator_simple { 0 } else { 2 },
                );
                if !numerator_simple {
                    fraction.push('(');
                }
                fraction.push_str(&numerator);
                if !numerator_simple {
                    fraction.push(')');
                }
                fraction.push('/');
                if !denominator_simple {
                    fraction.push('(');
                }
                fraction.push_str(&denominator);
                if !denominator_simple {
                    fraction.push(')');
                }
                return Some(Cow::Owned(fraction));
            }
            "sqrt" => {
                let (body, _) = self.argument(depth + 1)?;
                if body.is_empty() {
                    return None;
                }
                return Some(Cow::Owned(format!("√({body})")));
            }
            "text" | "mathrm" => {
                let body = self.text_argument(depth + 1, command == "mathrm")?;
                if self.source[self.pos..]
                    .trim_start_matches(char::is_whitespace)
                    .starts_with(['^', '_'])
                {
                    return None;
                }
                return Some(body);
            }
            "mathbb" => {
                self.skip_space();
                if self.take()? != '{' {
                    return None;
                }
                let letter = self.take()?;
                if self.take()? != '}' {
                    return None;
                }
                return Some(Cow::Borrowed(match letter {
                    'N' => "ℕ",
                    'Z' => "ℤ",
                    'Q' => "ℚ",
                    'R' => "ℝ",
                    'C' => "ℂ",
                    _ => return None,
                }));
            }
            _ => return None,
        };
        Some(Cow::Borrowed(symbol))
    }
}
