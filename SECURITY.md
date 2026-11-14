# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through [GitHub's private vulnerability reporting](https://github.com/ScarecrowAU/testudo/security/advisories/new). Do not open a public issue.

Include the Testudo version or commit, your OS, and steps to reproduce. You'll get a reply within a week. Fixes go into the next release, and the advisory is published once that release is out.

## Supported versions

Only the latest release gets security fixes.

## Verifying releases

Release archives carry a build provenance attestation from this repository's CI:

```sh
gh attestation verify testudo-<version>-<os>-<arch>.tar.gz --repo ScarecrowAU/testudo
```
