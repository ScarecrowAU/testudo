# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through [GitHub's private vulnerability reporting](https://github.com/ScarecrowAU/testudo/security/advisories/new). Do not open a public issue.

Include the Testudo version or commit, your OS, and steps to reproduce. You'll get a reply within a week. Fixes go into the next release, and the advisory is published once that release is out.

## Supported versions

Only the latest release gets security fixes.

## Verifying releases

Release archives are signed with minisign. Testudo's updater checks the signature before installing anything. To check one by hand:

```sh
minisign -V -P RWSmCfcnTJgsZrV6+H0NIc6Fi9I+ydhUDLazPG5hFYvAXpZtkmNcOTeJ -m testudo-<version>-<target>.tar.gz
```

Release archives also carry a build provenance attestation from this repository's CI:

```sh
gh attestation verify testudo-<version>-<target>.tar.gz --repo ScarecrowAU/testudo
```

The `bleeding-edge` update channel builds `main` from source over HTTPS instead, so it relies on GitHub and this repository's branch protection rather than a signature.
