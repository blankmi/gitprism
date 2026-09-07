# Concepts

* [0001-gitlab-pipeline-triggers](0001-gitlab-pipeline-triggers.md) - Recommended GitLab CI trigger setup for both sync directions. Operational guidance, not part of gitprism's own architecture.
* [0002-release-distribution](0002-release-distribution.md) - Release tag, platform archive, checksum and publication policy. Operational guidance, not part of gitprism's own architecture.
* [0003-recover-from-a-stale-source-checkout-refusal](0003-recover-from-a-stale-source-checkout-refusal.md) - Operator steps when decisions/0050 halts a mirror-only branch because the clone's local tip is not the source remote's tip: fast-forward a behind checkout, push or drop local-only commits, or fix the source query; then rerun. A deliberate source rewrite needs no operator step once the checkout matches source.
