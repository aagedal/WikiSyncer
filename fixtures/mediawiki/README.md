# MediaWiki fixtures

Recorded and hand-authored API responses used by `wikisync-mediawiki` tests belong
here. Recorded fixtures must not contain credentials, cookies, or private request
headers.

The integration-test harness serves these files over a loopback-only HTTP listener.
That exercises the production request builder, response limits, continuation tokens,
and retry classification while keeping the test suite deterministic and offline.

Fixtures should use obviously synthetic users, timestamps, and hashes unless the
exact upstream value is material to a parser regression. Keep one protocol behavior
per file and document unusual omitted or hidden fields in the consuming test.
