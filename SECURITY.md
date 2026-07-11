# Security Policy

## Sensitive material

The repository must never contain any of the followingค่ะ

- Google OAuth refresh or access tokensค่ะ
- OAuth client secrets intended to remain confidentialค่ะ
- Android signing keystores or their passwordsค่ะ
- Apple or Windows signing certificates and private keysค่ะ
- Real user vault contents or attachmentsค่ะ
- Local database files, logs containing note text, or crash dumps containing secretsค่ะ

Use local environment configuration and operating-system secure storage for credentialsค่ะ

## Logging

Logs must redact authorization headers, tokens, note bodies, attachment contents, and filesystem locations that expose private user informationค่ะ

## Reporting

Until a private security channel is established, do not publish exploitable security details in a public issueค่ะ

