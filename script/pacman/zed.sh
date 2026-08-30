#!/bin/sh
export ZED_UPDATE_EXPLANATION="Zed was installed via pacman; update with 'pacman -Syu'."
exec /usr/lib/zed-kjanat/bin/zed "$@"
