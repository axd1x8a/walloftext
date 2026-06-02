FROM rust:alpine AS base
RUN apk add --no-cache musl-dev

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY shared/Cargo.toml shared/Cargo.toml
COPY backend/Cargo.toml backend/Cargo.toml
COPY frontend/Cargo.toml frontend/Cargo.toml
COPY tools/font-gen/Cargo.toml tools/font-gen/Cargo.toml
COPY tools/video/Cargo.toml tools/video/Cargo.toml

RUN mkdir -p shared/src backend/src backend/src/bin frontend/src tools/font-gen/src tools/video/src \
    && touch shared/src/lib.rs frontend/src/lib.rs \
    && echo 'fn main(){}' > backend/src/main.rs \
    && echo 'fn main(){}' > backend/src/bin/migrate_snapshot.rs \
    && echo 'fn main(){}' > tools/font-gen/src/main.rs \
    && echo 'fn main(){}' > tools/video/src/main.rs

FROM base AS wasm-builder
RUN apk add --no-cache binaryen
RUN rustup target add wasm32-unknown-unknown
RUN cargo install wasm-pack

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wasm-target,target=/app/target \
    cargo build --release --target wasm32-unknown-unknown -p walloftext-frontend

COPY shared/src shared/src
COPY frontend/src frontend/src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wasm-target,target=/app/target \
    find shared/src frontend/src -name '*.rs' | xargs touch \
    && wasm-pack build frontend/ --target web --out-dir ../static --no-typescript --release

FROM base AS backend-builder
RUN apk add --no-cache openssl-dev

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=backend-target,target=/app/target \
    cargo build --release -p walloftext-backend

COPY shared/src shared/src
COPY backend/src backend/src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=backend-target,target=/app/target \
    find shared/src backend/src -name '*.rs' | xargs touch \
    && cargo build --release -p walloftext-backend \
    && cp target/release/walloftext /usr/local/bin/walloftext

FROM alpine:latest
RUN apk add --no-cache ca-certificates

WORKDIR /app

COPY --from=backend-builder /usr/local/bin/walloftext /app/walloftext
COPY ./static/index.html /app/static/index.html
COPY ./static/worker.js /app/static/worker.js
COPY ./static/unifont.wtfont /app/static/unifont.wtfont
COPY --from=wasm-builder /app/static/walloftext_frontend.js /app/static/walloftext_frontend.js
COPY --from=wasm-builder /app/static/walloftext_frontend_bg.wasm /app/static/walloftext_frontend_bg.wasm

CMD ["/app/walloftext"]
