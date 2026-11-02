# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
# Build only with build.py's credential-free, allowlisted context.
FROM ubuntu:26.04@sha256:c098726dd9b9733f3e885f8de0ef375206a311b0a317eadc40d347b1115fb944 AS desktop
ARG DEBIAN_FRONTEND=noninteractive
ARG DRIVER_SHA256
ARG BENCH_SOURCE_REVISION
LABEL org.opencontainers.image.description="Disposable Cua Bench v2 private X11 desktop (not a host desktop client)" \
      org.opencontainers.image.revision=$BENCH_SOURCE_REVISION
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl gnupg git tini \
    xvfb xauth dbus-x11 at-spi2-core openbox picom \
    python3 python3-gi python3-pytest python3-pil python3-venv \
    gir1.2-gtk-3.0 gir1.2-atspi-2.0 libglib2.0-bin \
    xdotool x11-utils libxss1 ffmpeg lsof procps \
    nodejs npm libreoffice-calc libreoffice-gtk3 gnucash \
    fonts-dejavu-core fonts-liberation libasound2t64 libnss3 libgbm1 \
    libgtk-3-0t64 libatk-bridge2.0-0t64 libdrm2 libxkbcommon0 \
    && rm -rf /var/lib/apt/lists/*
# CUA-1948: disable the roots of the compromised eMudhra intermediates before
# any HTTPS fetch. This is a no-op once ca-certificates drops the roots.
RUN sed -i -E 's#^(mozilla/(emSign_ECC_Root_CA_-_G3|Comodo_AAA_Services_root)\.crt)$#!\1#' /etc/ca-certificates.conf \
    && update-ca-certificates
# Ubuntu's chromium-browser requires snapd. Use Google's signed apt repository;
# the installed package/version is retained in packages.tsv.
RUN curl -fsSL https://dl.google.com/linux/linux_signing_key.pub \
      | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg \
    && printf '%s\n' 'deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] https://dl.google.com/linux/chrome/deb/ stable main' > /etc/apt/sources.list.d/google-chrome.list \
    && apt-get update && apt-get install -y --no-install-recommends google-chrome-stable \
    && rm -rf /var/lib/apt/lists/*
# The distro's GnuCash 5.14 is below the pack's >=5.16 requirement.
# Build the upstream 5.16 release from its published SHA256 in an isolated stage.
FROM desktop AS gnucash-build
RUN printf '%s\n' 'deb-src http://archive.ubuntu.com/ubuntu resolute main universe' > /etc/apt/sources.list.d/bench-source.list \
    && apt-get update && apt-get build-dep -y gnucash \
    && apt-get install -y --no-install-recommends ninja-build \
    && curl -fL https://github.com/Gnucash/gnucash/releases/download/5.16/gnucash-5.16.tar.bz2 -o /gnucash.tar.bz2 \
    && printf '%s  %s\n' bb9b6c67ebb6a3e06d9ff4f4e0417a97bef0159d26912be9b0e75a32901fd493 /gnucash.tar.bz2 | sha256sum -c - \
    && mkdir /gnucash-src && tar -xjf /gnucash.tar.bz2 --strip-components=1 -C /gnucash-src \
    && cmake -S /gnucash-src -B /gnucash-build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/gnucash \
    && cmake --build /gnucash-build -j 6 \
    && cmake --install /gnucash-build

FROM desktop AS runtime
ARG DRIVER_SHA256
ARG BENCH_SOURCE_REVISION
COPY --from=gnucash-build /opt/gnucash /opt/gnucash
COPY --from=gnucash-build /etc/gnucash /etc/gnucash
COPY containers/gnucash.sh /usr/local/bin/gnucash
RUN chmod 755 /usr/local/bin/gnucash \
    && printf '%s\n' /opt/gnucash/lib > /etc/ld.so.conf.d/bench-gnucash.conf \
    && ldconfig
COPY containers/app_defaults.py /opt/cua-container/app_defaults.py
RUN python3 /opt/cua-container/app_defaults.py
COPY cua-driver-local /usr/local/bin/cua-driver-local
RUN test -n "$DRIVER_SHA256" \
    && printf '%s  %s\n' "$DRIVER_SHA256" /usr/local/bin/cua-driver-local | sha256sum -c - \
    && chmod 755 /usr/local/bin/cua-driver-local \
    && ln -s cua-driver-local /usr/local/bin/cua-driver \
    && /usr/local/bin/cua-driver-local --version
COPY bench /opt/cua-bench
COPY skills /opt/cua-skills
# Install at build time, never during a trial. Fail on any app install failure.
RUN set -e; for app in /opt/cua-bench/taskpack/tasks/shared/*/apps/*; do \
      [ -f "$app/package-lock.json" ] || continue; \
      (cd "$app" && npm ci --no-audit --no-fund \
        && { [ -d node_modules/electron/dist ] || node node_modules/electron/install.js; }); \
    done \
    && rm -rf /root/.npm /root/.cache
COPY containers/entrypoint.sh /usr/local/bin/bench-container-entrypoint
COPY containers/npx.sh /usr/local/bin/npx
COPY containers/google-chrome.sh /usr/local/bin/google-chrome
COPY containers/smoke.py /opt/cua-container/smoke.py
COPY containers/pack_smoke.py /opt/cua-container/pack_smoke.py
COPY snapshot.json /opt/cua-container/snapshot.json
RUN chmod 755 /usr/local/bin/bench-container-entrypoint /usr/local/bin/npx /usr/local/bin/google-chrome \
    && mkdir -p /opt/cua-container /home/bench /evidence \
    && if getent passwd 1000; then userdel "$(getent passwd 1000 | cut -d: -f1)"; fi \
    && useradd --uid 1000 --home-dir /home/bench bench \
    && chown 1000:1000 /home/bench /evidence \
    && dpkg-query -W -f='${Package}\t${Version}\n' > /opt/cua-container/packages.tsv \
    && printf '%s  %s\n' "$DRIVER_SHA256" /usr/local/bin/cua-driver-local > /opt/cua-container/driver.sha256 \
    && printf '%s\n' "$BENCH_SOURCE_REVISION" > /opt/cua-container/source-revision
USER 1000:1000
WORKDIR /opt/cua-bench/automated-eval/macos_bench
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/bench-container-entrypoint"]
CMD ["/usr/bin/python3", "/opt/cua-container/smoke.py"]
