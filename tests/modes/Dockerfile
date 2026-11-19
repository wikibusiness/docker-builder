FROM busybox AS s
RUN mkdir -m 700 /src && echo x > /src/f && chown 5:5 /src
FROM busybox
RUN mkdir -m 700 /e1 /e2 /e3 && chown 7:7 /e2 && mkdir -p -m 700 /e4/sub && chown 7:7 /e4/sub && mkdir -m 700 /e && chown 7:7 /e && mkdir -m 700 /e5 /e6
COPY d/. /m1
COPY --chown=1000:1000 d/. /m2
COPY --chmod=640 d/. /m3
COPY d/. /e1
COPY --chown=1000:1000 d/. /e2
COPY --chmod=640 d/. /e3
COPY --from=s /src /m4
COPY --from=s --chown=1000:1000 /src /m5
COPY d /m6/
COPY d/f /m7/sub/
COPY --chown=1000:1000 d/f /m8/sub/
COPY --chown=1000:1000 d/f /e/n/
COPY --chown=1000:1000 d/f /m9
COPY --chmod=640 d/. /p/q
COPY --chmod=640 d/f /r/s/
COPY d/. /e4
COPY --chown=1000:1000 --chmod=600 d/. /u/v
COPY e /emp
COPY d/f /e5
COPY --from=s /src/f /e6
COPY d/. /x/.
COPY d/f /fl
WORKDIR /w
COPY d/f .
COPY d/sub/s ./t/
RUN stat -c "%a %u:%g %n" /m1 /m2 /m3 /e1 /e2 /e3 /m4 /m5 /m6 /m7 /m7/sub /m3/f /m8 /m8/sub /e /e/n /m9 /p /p/q /p/q/sub /r /r/s /r/s/f /e4 /e4/sub /u /u/v /u/v/sub /emp /e5 /e5/f /e6 /e6/f /x /x/f /x/sub /fl /w /w/f /w/t /w/t/s
