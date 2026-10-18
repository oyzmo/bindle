#!/bin/bash
#flatpak-builder build-dir no.oyzmo.Bindle.yml --force-clean
#flatpak-builder --user --force-clean --repo=repo build-dir
  flatpak-builder --user --force-clean --repo=repo build-dir no.oyzmo.Bindle.yml
  flatpak build-bundle repo bindle-1.0.2.flatpak no.oyzmo.Bindle
