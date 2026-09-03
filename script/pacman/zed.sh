#!/bin/sh
export ZED_UPDATE_EXPLANATION="Zed was installed via pacman."
export ZED_UPDATE_COMMAND="pacman -Syu"
exec /usr/lib/zed-kjanat/bin/zed "$@"
