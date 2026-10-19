# FailForge configuration

Native normalization for direct mutation settings and explicit YAML profiles.
The library performs no SDK readiness checks, dependency installation, discovery
or author-test execution. It retains the current profile activation rules,
canonical operator ordering, path bases and separate test/command timeout policy.

`Context::new` binds the invocation directory and installation. `resolve_config`
accepts semantic CLI settings, `load_yaml` loads one explicit profile, and
`from_mapping` / `from_profile_mapping` support programmatic callers. Effective
typed models expose fresh JSON projections for manifests. During the engine
migration, `RunConfig::to_transport_value` also carries each selected campaign to
the remaining execution controller without parsing YAML a second time.

YAML input is bounded to 1 MiB, 32 collection levels and 65,536 decoded characters
per scalar. Mapping keys must be strings and unique. Anchors, aliases, explicit
tags and multiple documents are rejected. Scalar types retain the previous YAML
1.1 configuration semantics, including strict lowercase `true` / `false`.

Run the unit tests and build the measurement driver:

```sh
cargo test -p failforge-config --all-targets --locked
cargo build --release -p failforge-config --example config_bench --locked
```

The driver reads one JSON request per line with `operation` (`cli`, `yaml`,
`mapping` or `profile`), `value`, `cwd`, `installation` and optional `iterations`.
Profile mappings additionally use `project`; mapping APIs accept `base_dir`.
Each iteration includes context/path resolution, parsing or normalization and a
complete effective JSON projection. Response encoding happens after the timed
window. Compare a release binary and the original implementation using identical
inputs, matching accepted/rejected outcomes and effective configurations first.
Measure process startup separately from repeated calls; retain raw wall-time,
CPU and peak-RSS observations. Configuration timings do not measure Dart tests
or the memory consumption of an entire mutation campaign.
