"""Generate the two test-only private-handler wrappers from an audited checkout.

Usage: python3 prepare.py /absolute/path/to/anytls-rs
Production sources are read only. All generated files stay beside this script.
"""
from pathlib import Path
import json
import sys

here = Path(__file__).resolve().parent
repo = Path(sys.argv[1]).resolve()
here.joinpath('src').mkdir(exist_ok=True)
for filename, function in [('http_proxy', 'handle_http_proxy_connection'), ('socks5', 'handle_socks5_connection')]:
    source = repo.joinpath('src/client', filename + '.rs').read_text()
    wrapper = f'\npub async fn audit_handle(conn: tokio::net::TcpStream, client: std::sync::Arc<Client>) -> Result<()> {{ {function}(conn, client).await }}\n'
    here.joinpath('src', filename + '_probe.rs').write_text(source + wrapper)
here.joinpath('Cargo.toml').write_text('''[package]
name = "client-audit-repros"
version = "0.1.0"
edition = "2024"
[dependencies]
anytls-rs = { path = REPO_PATH }
tokio = { version = "1", features = ["full", "test-util"] }
tokio-rustls = "0.26"
bytes = "1"
tracing = "0.1"
anyhow = "1"
'''.replace('REPO_PATH', json.dumps(str(repo))))
print('Prepared wrappers and path dependency for', repo)
