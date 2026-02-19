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
