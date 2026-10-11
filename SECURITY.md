# Security reports

For a vulnerability involving camera firmware writes or package validation,
use GitHub's private vulnerability reporting for this repository when enabled.
Otherwise open an issue that describes the impact without posting exploit
payloads or account secrets.

The application accepts only official S3 firmware metadata/package URLs and
verifies the archive before upload. It does not extract firmware files locally
or require camera Telnet/FTP credentials. Checksums detect corruption; these
vendor packages do not supply an independently verified publisher signature.
