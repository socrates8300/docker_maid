#!/usr/bin/env bash
set -e
echo "guest green run 1"
uname -a
docker --version
echo "sleeping to expose the supersede window"
sleep 40
echo "GUEST-GREEN-1"
