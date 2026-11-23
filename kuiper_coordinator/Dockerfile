# Phoenix coordinator image: a drop-in swap for the Rust coordinator's image
# (kuiper-forge/docker/Dockerfile). Same /data volume, same non-root kuiper user (uid/gid 999),
# same agent port (9443) and TLS file names, so a Rust deployment's env block and volume carry
# over (D72, D73). See docs/docker-swap.md.
#
#   docker build -t kuiper-coordinator kuiper_coordinator
#   docker run IMAGE                         migrate, then serve (the default)
#   docker run IMAGE migrate                 only migrate (creates the database if it's missing)
#   docker run IMAGE import_rust --sqlite /data/kuiper.db [--dry-run]
#   docker run IMAGE server                  serve without migrating

# keep in sync with mise.toml and .github/workflows/phoenix.yml
ARG ELIXIR_VERSION=1.19.6
ARG OTP_VERSION=28.5.0.8
ARG DEBIAN_VERSION=trixie-20261027-slim
ARG BUILDER_IMAGE="hexpm/elixir:${ELIXIR_VERSION}-erlang-${OTP_VERSION}-debian-${DEBIAN_VERSION}"
ARG RUNNER_IMAGE="debian:${DEBIAN_VERSION}"

FROM ${BUILDER_IMAGE} AS builder

RUN apt-get update && apt-get install -y --no-install-recommends build-essential git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
RUN mix local.hex --force && mix local.rebar --force
ENV MIX_ENV=prod

COPY mix.exs mix.lock ./
RUN mix deps.get --only $MIX_ENV
RUN mkdir config
COPY config/config.exs config/${MIX_ENV}.exs config/
RUN mix deps.compile

# tailwind and esbuild binaries, before the app's assets
RUN mix assets.setup

COPY priv priv
COPY lib lib
RUN mix compile
COPY assets assets
RUN mix assets.deploy

COPY config/runtime.exs config/
COPY rel rel
RUN mix release

# --- Runtime ---
# docker.yml never builds this stage from cache, so each release installs the current
# ca-certificates (Req and the forge APIs trust the system store)
FROM ${RUNNER_IMAGE} AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends libstdc++6 openssl libncurses6 locales ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && sed -i '/en_US.UTF-8/s/^# //g' /etc/locale.gen && locale-gen

ENV LANG=en_US.UTF-8 LANGUAGE=en_US:en LC_ALL=en_US.UTF-8

# the Rust image's user: `useradd --system` there gets 999, so files it wrote to /data stay readable
RUN groupadd --system --gid 999 kuiper \
    && useradd --system --uid 999 --gid kuiper --create-home kuiper \
    && mkdir -p /data && chown kuiper:kuiper /data

WORKDIR /app
COPY --from=builder --chown=kuiper:kuiper /app/_build/prod/rel/kuiper_coordinator ./
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh

USER kuiper

ENV MIX_ENV=prod
# CA, server cert and key, and the Management socket: the Rust image's volume and file names.
# The Rust image's KUIPER_TLS__* paths work too (D73).
ENV KUIPER_DATA_DIR=/data
# agents dial 9443 on the Rust image; Phoenix takes Rust's listen address as its agent port
# (KUIPER_AGENT_PORT wins when set). The admin UI and webhooks are on PORT (9480).
ENV KUIPER_GRPC__LISTEN_ADDR=0.0.0.0:9443
ENV PORT=9480

VOLUME /data
EXPOSE 9443 9480

ENTRYPOINT ["entrypoint.sh"]
CMD ["start"]
