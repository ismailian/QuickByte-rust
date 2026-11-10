//! HTTP Digest authentication (RFC 7616), and the `WWW-Authenticate` parsing
//! every challenge-response scheme shares.
//!
//! The C# build had `SocketsHttpHandler` answer Digest itself once its
//! `Credentials` held the login; here the answer is computed in Rust. MD5,
//! SHA-256 and their `-sess` variants are supported, with `qop=auth` (and
//! `auth-int`, whose body hash is that of the empty body every request here
//! has). Digest answers a nonce the server issued, which is why it can never be
//! sent up front the way Basic is.

use md5::Md5;
use sha2::{Digest as _, Sha256};
use url::Url;

/// One challenge from a `WWW-Authenticate` header: its scheme, and either its
/// parameters (`realm="x", nonce="y"`) or a single token (NTLM's base64 blob).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: String,
    pub params: Vec<(String, String)>,
    pub token: Option<String>,
}

impl Challenge {
    /// A parameter, by case-insensitive name.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn is(&self, scheme: &str) -> bool {
        self.scheme.eq_ignore_ascii_case(scheme)
    }
}

/// Every challenge in every `WWW-Authenticate` value, in order.
pub fn parse_challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    let mut challenges = Vec::new();
    for value in values {
        parse_into(value, &mut challenges);
    }
    challenges
}

fn parse_into(value: &str, out: &mut Vec<Challenge>) {
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    let skip = |i: &mut usize, also_commas: bool| {
        while *i < chars.len() && (chars[*i].is_whitespace() || (also_commas && chars[*i] == ',')) {
            *i += 1;
        }
    };
    let read_token = |i: &mut usize| {
        let start = *i;
        while *i < chars.len() && !chars[*i].is_whitespace() && chars[*i] != ',' && chars[*i] != '=' {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>()
    };

    loop {
        skip(&mut i, true);
        if i >= chars.len() {
            return;
        }
        let scheme = read_token(&mut i);
        if scheme.is_empty() {
            i += 1;
            continue;
        }
        let mut challenge = Challenge { scheme, ..Default::default() };

        loop {
            skip(&mut i, false);
            let at_comma = i < chars.len() && chars[i] == ',';
            if at_comma {
                // A comma either separates this challenge's parameters or starts
                // the next challenge: a parameter is "name=" right after it.
                let mut look = i;
                skip(&mut look, true);
                let mut probe = look;
                let name = read_token(&mut probe);
                let mut after = probe;
                skip(&mut after, false);
                let is_param = !name.is_empty() && after < chars.len() && chars[after] == '=' && challenge.token.is_none();
                if !is_param {
                    i = look;
                    break;
                }
                i = look;
            }
            if i >= chars.len() {
                break;
            }

            let start = i;
            let name = read_token(&mut i);
            let mut after = i;
            skip(&mut after, false);
            if after < chars.len() && chars[after] == '=' {
                // token68 ends in '=' padding: "abc==" followed by nothing or a comma.
                let mut pad = after;
                while pad < chars.len() && chars[pad] == '=' {
                    pad += 1;
                }
                let mut rest = pad;
                skip(&mut rest, false);
                let nothing_follows = rest >= chars.len() || chars[rest] == ',';
                if challenge.params.is_empty() && challenge.token.is_none() && nothing_follows {
                    challenge.token = Some(chars[start..pad].iter().collect());
                    i = pad;
                    continue;
                }
                i = after + 1;
                skip(&mut i, false);
                let value = read_param_value(&chars, &mut i);
                challenge.params.push((name, value));
            } else if !name.is_empty() {
                if challenge.params.is_empty() && challenge.token.is_none() {
                    challenge.token = Some(name);
                } else {
                    // Not a parameter and not a token: the next challenge's scheme.
                    i = start;
                    break;
                }
            } else {
                i += 1;
            }
        }
        out.push(challenge);
    }
}

fn read_param_value(chars: &[char], i: &mut usize) -> String {
    if *i < chars.len() && chars[*i] == '"' {
        *i += 1;
        let mut value = String::new();
        while *i < chars.len() {
            match chars[*i] {
                '\\' if *i + 1 < chars.len() => {
                    value.push(chars[*i + 1]);
                    *i += 2;
                }
                '"' => {
                    *i += 1;
                    break;
                }
                c => {
                    value.push(c);
                    *i += 1;
                }
            }
        }
        value
    } else {
        let start = *i;
        while *i < chars.len() && chars[*i] != ',' && !chars[*i].is_whitespace() {
            *i += 1;
        }
        chars[start..*i].iter().collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algorithm {
    Md5,
    Sha256,
}

impl Algorithm {
    fn hash(self, text: &str) -> String {
        let bytes = match self {
            Self::Md5 => Md5::digest(text.as_bytes()).to_vec(),
            Self::Sha256 => Sha256::digest(text.as_bytes()).to_vec(),
        };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A Digest login, answering successive challenges from one server (the nonce
/// count rises with each answer to the same nonce).
pub struct DigestSession {
    user_name: String,
    password: String,
    nonce: String,
    count: u32,
}

impl DigestSession {
    pub fn new(user_name: &str, password: &str) -> Self {
        Self { user_name: user_name.to_string(), password: password.to_string(), nonce: String::new(), count: 0 }
    }

    /// The `Authorization` value answering `challenge` for `method` on `url`, or
    /// `None` for a Digest variant this cannot compute.
    pub fn answer(&mut self, challenge: &Challenge, method: &str, url: &Url) -> Option<String> {
        self.answer_with_cnonce(challenge, method, url, &new_cnonce())
    }

    fn answer_with_cnonce(&mut self, challenge: &Challenge, method: &str, url: &Url, cnonce: &str) -> Option<String> {
        let realm = challenge.param("realm").unwrap_or("");
        let nonce = challenge.param("nonce")?;
        let opaque = challenge.param("opaque");
        let algorithm_name = challenge.param("algorithm").unwrap_or("MD5");
        let (algorithm, session) = match algorithm_name.to_ascii_uppercase().as_str() {
            "MD5" => (Algorithm::Md5, false),
            "MD5-SESS" => (Algorithm::Md5, true),
            "SHA-256" => (Algorithm::Sha256, false),
            "SHA-256-SESS" => (Algorithm::Sha256, true),
            _ => return None,
        };

        let offered: Vec<String> = challenge.param("qop").unwrap_or("").split(',').map(|q| q.trim().to_ascii_lowercase()).collect();
        let qop = if offered.iter().any(|q| q == "auth") {
            Some("auth")
        } else if offered.iter().any(|q| q == "auth-int") {
            Some("auth-int")
        } else {
            None
        };

        if self.nonce != nonce {
            self.nonce = nonce.to_string();
            self.count = 0;
        }
        self.count += 1;
        let nc = format!("{:08x}", self.count);

        let uri = request_target(url);
        let mut ha1 = algorithm.hash(&format!("{}:{realm}:{}", self.user_name, self.password));
        if session {
            ha1 = algorithm.hash(&format!("{ha1}:{nonce}:{cnonce}"));
        }
        let ha2 = match qop {
            Some("auth-int") => algorithm.hash(&format!("{method}:{uri}:{}", algorithm.hash(""))),
            _ => algorithm.hash(&format!("{method}:{uri}")),
        };
        let response = match qop {
            Some(q) => algorithm.hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{q}:{ha2}")),
            None => algorithm.hash(&format!("{ha1}:{nonce}:{ha2}")),
        };

        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm={algorithm_name}, response=\"{response}\"",
            quote(&self.user_name),
            quote(realm),
            quote(nonce),
            quote(&uri)
        );
        if let Some(q) = qop {
            header.push_str(&format!(", qop={q}, nc={nc}, cnonce=\"{cnonce}\""));
        }
        if let Some(opaque) = opaque {
            header.push_str(&format!(", opaque=\"{}\"", quote(opaque)));
        }
        Some(header)
    }
}

/// The request-target Digest hashes: the path and query of the address.
fn request_target(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

fn quote(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn new_cnonce() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_are_read_with_their_parameters() {
        let parsed = parse_challenges([
            "Digest realm=\"files\", qop=\"auth\", nonce=\"abc\", algorithm=MD5",
            "Basic realm=\"files\"",
        ]);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is("digest"));
        assert_eq!(parsed[0].param("realm"), Some("files"));
        assert_eq!(parsed[0].param("NONCE"), Some("abc"));
        assert_eq!(parsed[0].param("algorithm"), Some("MD5"));
        assert!(parsed[1].is("Basic"));
    }

    #[test]
    fn several_challenges_in_one_header_are_told_apart() {
        let parsed = parse_challenges(["Negotiate, NTLM, Basic realm=\"x\", charset=\"UTF-8\""]);
        let schemes: Vec<&str> = parsed.iter().map(|c| c.scheme.as_str()).collect();
        assert_eq!(schemes, vec!["Negotiate", "NTLM", "Basic"]);
        assert_eq!(parsed[2].param("charset"), Some("UTF-8"));
    }

    #[test]
    fn an_ntlm_token_is_read_whole_padding_and_all() {
        let parsed = parse_challenges(["NTLM TlRMTVNTUAACAAAADAAMADgAAAA="]);
        assert_eq!(parsed[0].token.as_deref(), Some("TlRMTVNTUAACAAAADAAMADgAAAA="));
        let parsed = parse_challenges(["NTLM"]);
        assert_eq!(parsed[0].token, None);
    }

    #[test]
    fn the_rfc_2617_example_is_answered_correctly() {
        // RFC 2617 §3.5: Mufasa / "Circle Of Life" on /dir/index.html.
        let challenge = parse_challenges([
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
        ])
        .remove(0);
        let mut session = DigestSession::new("Mufasa", "Circle Of Life");
        let url = Url::parse("http://www.nowhere.org/dir/index.html").unwrap();
        let header = session.answer_with_cnonce(&challenge, "GET", &url, "0a4f113b").unwrap();
        assert!(header.contains("response=\"6629fae49393a05397450978507c4ef1\""), "{header}");
        assert!(header.contains("nc=00000001"));
        assert!(header.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
    }

    #[test]
    fn the_nonce_count_rises_for_the_same_nonce() {
        let challenge = parse_challenges(["Digest realm=\"r\", qop=\"auth\", nonce=\"n\""]).remove(0);
        let mut session = DigestSession::new("u", "p");
        let url = Url::parse("http://h/x").unwrap();
        session.answer(&challenge, "GET", &url).unwrap();
        assert!(session.answer(&challenge, "GET", &url).unwrap().contains("nc=00000002"));
    }

    #[test]
    fn an_unknown_algorithm_is_not_guessed_at() {
        let challenge = parse_challenges(["Digest realm=\"r\", nonce=\"n\", algorithm=SHA-512-256"]).remove(0);
        assert_eq!(DigestSession::new("u", "p").answer(&challenge, "GET", &Url::parse("http://h/").unwrap()), None);
    }
}
