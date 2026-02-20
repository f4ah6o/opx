# Release new version (tag + push)

release-check:
    cargo test --all --all-features
    cargo build --release --all-features
    cargo publish --dry-run

release: release-check
    version=$(rg -n "^version = " Cargo.toml | head -n1 | awk -F'\"' '{print $2}'); \
    git tag "v${version}"; \
    git push origin "v${version}"

e2e:
    OPZ_E2E=1 cargo test --test e2e_real_op -- --nocapture

e2e-trace:
    OPZ_GIT_COMMIT=$(git rev-parse --short=12 HEAD) OTEL_SERVICE_NAME=opz-e2e OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 OPZ_E2E=1 cargo test --test e2e_real_op -- --nocapture

trace-report ref service='opz-e2e' limit='200':
    python3 scripts/jaeger_trace_compare.py --service {{service}} --limit {{limit}} report --commit {{ref}}

trace-compare base head service='opz-e2e' limit='200':
    python3 scripts/jaeger_trace_compare.py --service {{service}} --limit {{limit}} compare --base {{base}} --head {{head}}

jaeger-up:
    docker compose up -d

jaeger-down:
    docker compose down

trace-find query='example':
    OPZ_GIT_COMMIT=$(git rev-parse --short=12 HEAD) OTEL_SERVICE_NAME=opz OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run -- find {{query}}

trace-run item='example-item':
    OPZ_GIT_COMMIT=$(git rev-parse --short=12 HEAD) OTEL_SERVICE_NAME=opz OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run -- run {{item}} -- env

trace-ui:
    (open http://localhost:16686 || xdg-open http://localhost:16686 || echo "Open http://localhost:16686 in your browser")
