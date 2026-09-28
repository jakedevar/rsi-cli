#!/bin/sh
while [ "$1" != -- ]; do shift; done
shift
exec "$@"
