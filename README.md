# SessionDriver

A portable proxy to manage single-session WebDrivers (e.g. [geckodriver](https://github.com/mozilla/geckodriver/issues/2209)),
allowing for simultaneous connections.
Before using a client or acquiring a client from a pool, ensure the session exists. Unless explicitly 
specified, browsers are shut down after 12 hours of inactivity. For greater flexibility, please see 
[`Dockerfile-SessionDriver-Firefox`](./Dockerfile-SessionDriver-Firefox) and [`Dockerfile`](./Dockerfile). [`Dockerfile`](./Dockerfile)
exposes executables for `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`.

Please see an example of how to use SessionDriver with Rust at [`./src/lib.rs`](./src/lib.rs). As you might 
notice, an additional, non-spec conforming route (`/session/driver/{uuid}/status`) is exposed to check the
status of a managed session.

## Containerisation

```zsh
docker run -d -p 4444:4444 goslar/sessiondriver-firefox
```

### Dockerfile

```Dockerfile
FROM goslar/sessiondriver:latest AS session-driver
FROM alpine:3 AS gecko-fetcher

WORKDIR /build

RUN apk update
RUN apk add wget tar
RUN apk add ca-certificates
RUN wget https://github.com/mozilla/geckodriver/releases/download/v0.36.0/geckodriver-v0.36.0-linux64.tar.gz
RUN tar -xvzf geckodriver*
RUN chmod +x geckodriver

FROM jlesage/firefox:v25.03.1

RUN apk update && \
      apk add wget

COPY --from=gecko-fetcher /build/geckodriver /bin/geckodriver
COPY --from=session-driver /usr/local/bin/sessiondriver-musl /bin/sessiondriver

HEALTHCHECK --interval=15s --timeout=10s --retries=3 CMD wget --spider -q http://127.0.0.1:4444/status || exit 1

EXPOSE 4444

ENTRYPOINT [ "sessiondriver" ]
CMD [ "--port=4444", "--host=0.0.0.0", "--webdriver=geckodriver", "--parameters=--allow-hosts=*" ]
```