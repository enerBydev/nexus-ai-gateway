# Security Policy

## Reporting a Vulnerability
Please report security vulnerabilities by emailing the maintainer directly.
Do NOT file public issues for security vulnerabilities.

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 0.13.x  | ✅ Active |
| < 0.13  | ❌         |

## Security Features
- API keys stored in .env files (never hardcoded)
- SSRF protection in WebFetch (RFC1918 IP blocking)
- CORS defaults to localhost only
- Circuit breaker for cascade failure prevention
- .env file permissions set to 600 on Unix
