// Copyright 2026 John "topjohnwu" Wu
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use reqwest::blocking::{Client, ClientBuilder};
use reqwest::redirect::Policy;
use std::time::Duration;

const MAX_REDIRECTS: usize = 10;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Creates the common FUS HTTP client builder. Every request path must use
/// this builder so a compromised endpoint cannot downgrade a redirect to
/// plaintext HTTP and expose authorization headers or firmware bytes.
pub(crate) fn client_builder() -> ClientBuilder {
    Client::builder()
        // The blocking client resets this bound for each request setup and
        // successful body read, so active multi-gigabyte transfers may run for
        // as long as needed while stalled connections remain bounded.
        .timeout(IO_TIMEOUT)
        .connect_timeout(IO_TIMEOUT)
        .redirect(Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                let target = attempt.url().clone();
                return attempt.error(format!("refusing insecure redirect target: {target}"));
            }

            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn rejects_plaintext_http_redirect_targets() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/start", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/downgrade\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let error = client_builder()
            .build()
            .unwrap()
            .get(endpoint)
            .send()
            .unwrap_err();

        server.join().unwrap();
        assert!(error.is_redirect());
    }

    #[test]
    fn blocking_timeout_resets_after_each_successful_body_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/slow", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\na")
                .unwrap();
            stream.flush().unwrap();
            for byte in b"bcdef" {
                thread::sleep(Duration::from_millis(150));
                stream.write_all(&[*byte]).unwrap();
                stream.flush().unwrap();
            }
        });

        let client = client_builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let started = Instant::now();
        let mut response = client.get(endpoint).send().unwrap();
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();

        server.join().unwrap();
        assert_eq!(body, b"abcdef");
        assert!(started.elapsed() > Duration::from_millis(500));
    }
}
