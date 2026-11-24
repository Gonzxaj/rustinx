//! Extracts explicit HTTP server names from the files printed by nginx -T.

use std::collections::HashMap;

use thiserror::Error;

const FILE_HEADER: &str = "# configuration file ";
const USER_CONFIG_DIR: &str = "/etc/nginx/user_conf.d/";
const REDIRECT_SNIPPET: &str = "/etc/rustinx/snippets/managed-redirect.conf";

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("could not parse the effective Nginx configuration")]
    Syntax,
    #[error("managed HTTPS requires absolute Nginx includes; found {0:?}")]
    RelativeInclude(String),
}

#[derive(Debug, Default)]
pub(crate) struct Site {
    pub names: Vec<String>,
    pub redirect_enabled: bool,
    pub http: bool,
    pub https: bool,
}

pub(crate) fn discover(config: &str) -> Result<Vec<Site>, DiscoveryError> {
    let mut files = config.split_inclusive('\n').peekable();
    let mut sites = Vec::new();
    let mut is_main = true;
    while let Some(line) = files.next() {
        let Some(path) = line
            .trim_end()
            .strip_prefix(FILE_HEADER)
            .and_then(|s| s.strip_suffix(':'))
        else {
            continue;
        };
        let mut content = String::new();
        while files.peek().is_some_and(|line| !line.starts_with(FILE_HEADER)) {
            content.push_str(files.next().expect("peeked line"));
        }
        let mut stack = Vec::new();
        let mut args = Vec::new();
        let mut site = None;
        let mut lexer = Lexer {
            chars: content.chars().peekable(),
        };
        while let Some(token) = lexer.next()? {
            match token {
                Token::Word(word) => args.push(word),
                Token::Open => {
                    let name = args.first().ok_or(DiscoveryError::Syntax)?;
                    let in_http = if is_main {
                        stack == ["http"]
                    } else {
                        path.starts_with(USER_CONFIG_DIR) && stack.is_empty()
                    };
                    if name == "server" && in_http {
                        site = Some((stack.len() + 1, Site::default(), false));
                    }
                    stack.push(name.clone());
                    args.clear();
                }
                Token::Close => {
                    if stack.is_empty() || !args.is_empty() {
                        return Err(DiscoveryError::Syntax);
                    }
                    if site.as_ref().is_some_and(|(depth, _, _)| *depth == stack.len()) {
                        let (_, mut completed, explicit_listen) = site.take().expect("active server");
                        completed.http |= !explicit_listen;
                        completed.names.sort_unstable();
                        completed.names.dedup();
                        sites.push(completed);
                    }
                    stack.pop();
                }
                Token::End => {
                    if let [name, path] = args.as_slice()
                        && name == "include"
                        && !path.starts_with('/')
                    {
                        return Err(DiscoveryError::RelativeInclude(path.clone()));
                    }
                    if let Some((depth, site, explicit_listen)) = &mut site
                        && *depth == stack.len()
                    {
                        match args.as_slice() {
                            [name, values @ ..] if name == "server_name" => {
                                site.names
                                    .extend(values.iter().map(|value| value.to_ascii_lowercase()));
                            }
                            [name, addr, options @ ..] if name == "listen" => {
                                *explicit_listen = true;
                                let port = addr.rsplit(':').next().unwrap_or(addr);
                                let tls = options.iter().any(|value| value == "ssl" || value == "quic");
                                site.http |= port == "80" && !tls;
                                site.https |= port == "443";
                            }
                            [name, path] if name == "include" && path == REDIRECT_SNIPPET => {
                                site.redirect_enabled = true;
                            }
                            _ => {}
                        }
                    }
                    args.clear();
                }
            }
        }
        if !stack.is_empty() || !args.is_empty() {
            return Err(DiscoveryError::Syntax);
        }
        is_main = false;
    }
    if is_main {
        return Err(DiscoveryError::Syntax);
    }
    Ok(sites)
}

pub(crate) fn by_name(sites: &[Site]) -> HashMap<&str, Vec<&Site>> {
    let mut names = HashMap::<_, Vec<_>>::new();
    for site in sites {
        for name in &site.names {
            names.entry(name.as_str()).or_default().push(site);
        }
    }
    names
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Open,
    Close,
    End,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Lexer<'_> {
    fn next(&mut self) -> Result<Option<Token>, DiscoveryError> {
        let first = loop {
            match self.chars.next() {
                None => return Ok(None),
                Some('#') => while self.chars.next().is_some_and(|ch| ch != '\n') {},
                Some(ch) if ch.is_whitespace() => {}
                Some(ch) => break ch,
            }
        };
        let token = match first {
            '{' => Token::Open,
            '}' => Token::Close,
            ';' => Token::End,
            '\'' | '"' => Token::Word(self.quoted(first)?),
            _ => {
                let mut word = String::new();
                self.push_word_char(&mut word, first)?;
                while self
                    .chars
                    .peek()
                    .is_some_and(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}' | ';'))
                {
                    let ch = self.chars.next().expect("peeked character");
                    self.push_word_char(&mut word, ch)?;
                }
                Token::Word(word)
            }
        };
        Ok(Some(token))
    }

    fn quoted(&mut self, quote: char) -> Result<String, DiscoveryError> {
        let mut word = String::new();
        while let Some(ch) = self.chars.next() {
            match ch {
                '\\' => word.push(self.chars.next().ok_or(DiscoveryError::Syntax)?),
                ch if ch == quote => return Ok(word),
                ch => word.push(ch),
            }
        }
        Err(DiscoveryError::Syntax)
    }

    fn push_word_char(&mut self, word: &mut String, ch: char) -> Result<(), DiscoveryError> {
        match ch {
            '\\' => word.push(self.chars.next().ok_or(DiscoveryError::Syntax)?),
            '$' if self.chars.next_if_eq(&'{').is_some() => {
                word.push_str("${");
                loop {
                    let ch = self.chars.next().ok_or(DiscoveryError::Syntax)?;
                    word.push(ch);
                    if ch == '}' {
                        break;
                    }
                }
            }
            ch => word.push(ch),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_http_blocks_and_aliases_without_reading_nested_locations_as_servers() {
        let config = r#"
# configuration file /etc/rustinx/nginx.conf:
http { server { listen 127.0.0.1:8081; } }
# configuration file /etc/nginx/user_conf.d/example.conf:
server {
    listen [::]:80;
    server_name 'EXAMPLE.test' www.example.test;
    include /etc/rustinx/snippets/managed-redirect.conf;
    location ~ "a{1,2}" { return 200 "${host} #{x};"; }
}
server { listen 80; server_name example.test; }
server { listen 443 ssl; server_name existing.example.test; }
"#;
        let sites = discover(config).unwrap();
        let names = by_name(&sites);
        assert_eq!(names["example.test"].len(), 2);
        assert!(names["www.example.test"][0].redirect_enabled);
        assert!(names["www.example.test"][0].http);
        assert!(names["existing.example.test"][0].https);
    }
}
