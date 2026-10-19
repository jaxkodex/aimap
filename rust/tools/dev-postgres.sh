#!/bin/sh
# Stand up a disposable PostgreSQL 16 for the Rust test suite, without root.
# Binaries + data live under /tmp, so this survives nothing and owns nothing.
#
#   tools/dev-postgres.sh start|stop|env
#
# After `start`, run the tests with:
#   TEST_DATABASE_URL=postgresql://postgres@127.0.0.1:5433/postgres cargo test

set -e
PG_HOME="${PG_HOME:-$HOME/.aimap-pg}"
VERSION=16.4.0
ARCH=x86_64-unknown-linux-gnu
DIST="$PG_HOME/postgresql-$VERSION-$ARCH"
export LD_LIBRARY_PATH="$DIST/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

fetch_libs() {
    # The standalone postgres build needs libxml2/libicu/libreadline, which the
    # minimal images lack; extract them from the Debian pool without root.
    mkdir -p "$PG_HOME/sysroot"
    cd "$PG_HOME/sysroot"
    for spec in \
        "libx/libxml2 libxml2_2.9.14+dfsg-1.3~deb12u6_amd64.deb" \
        "r/readline libreadline8_8.2-1.3_amd64.deb" \
        "i/icu libicu72_72.1-3+deb12u1_amd64.deb"; do
        set -- $spec
        [ -f "$2" ] || curl -sSfL -O "https://deb.debian.org/debian/pool/main/$1/$2"
        dpkg -x "$2" .
    done
    export LD_LIBRARY_PATH="$PG_HOME/sysroot/usr/lib/x86_64-linux-gnu:$LD_LIBRARY_PATH"
}

case "${1:-start}" in
start)
    [ -d "$DIST" ] || {
        mkdir -p "$PG_HOME" && cd "$PG_HOME"
        curl -sSfL -o pg.tar.gz "https://github.com/theseus-rs/postgresql-binaries/releases/download/$VERSION/postgresql-$VERSION-$ARCH.tar.gz"
        tar xzf pg.tar.gz && rm pg.tar.gz
    }
    fetch_libs
    export PATH="$DIST/bin:$PATH"
    [ -d "$PG_HOME/data" ] || initdb -D "$PG_HOME/data" -U postgres --auth=trust -E UTF8 >/dev/null
    mkdir -p "$PG_HOME/sock"
    pg_ctl -D "$PG_HOME/data" -l "$PG_HOME/pg.log" \
        -o "-p 5433 -k $PG_HOME/sock -c listen_addresses=127.0.0.1" start
    sleep 1
    psql "postgresql://postgres@127.0.0.1:5433/postgres" -tAc "SELECT 'postgres up: ' || version();"
    ;;
stop)
    export PATH="$DIST/bin:$PATH"
    pg_ctl -D "$PG_HOME/data" stop || true
    ;;
env)
    echo "TEST_DATABASE_URL=postgresql://postgres@127.0.0.1:5433/postgres"
    echo "LD_LIBRARY_PATH=$PG_HOME/sysroot/usr/lib/x86_64-linux-gnu:$DIST/lib"
    ;;
*)
    echo "usage: $0 start|stop|env" >&2
    exit 2
    ;;
esac
