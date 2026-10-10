#!/bin/sh
export ZED_UPDATE_EXPLANATION="Zed was installed via pacman as zed-kjanat."
export ZED_UPDATE_COMMAND="sudo pacman -Syu zed-kjanat"
exec /usr/lib/zed-kjanat/bin/zed "$@"
