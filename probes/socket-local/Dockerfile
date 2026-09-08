FROM ubuntu:resolute@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    dbus-x11 ffmpeg grim labwc lxqt-session qterminal qt6-wayland wayland-utils wlr-randr \
    breeze-cursor-theme fonts-dejavu-core jq procps python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN usermod --login sprite --home /home/sprite --move-home ubuntu && groupmod --new-name sprite ubuntu
COPY target/release/sprite-desktop-gateway /opt/socket-local/bin/sprite-desktop-gateway
COPY target/release/sprite-desktop-streamd /opt/socket-local/bin/sprite-desktop-streamd
COPY probes/socket-local/start.sh probes/socket-local/session.sh probes/socket-local/key-fixture.sh probes/socket-local/key-fixture.py probes/socket-local/container-entrypoint.sh probes/socket-local/native.py /opt/socket-local/
COPY installer/session.py /opt/socket-local/session_owner.py
RUN chmod 0755 /opt/socket-local/bin/* /opt/socket-local/*.sh /opt/socket-local/*.py && chown -R sprite:sprite /opt/socket-local /home/sprite
USER sprite
WORKDIR /home/sprite
ENTRYPOINT ["/opt/socket-local/start.sh"]
