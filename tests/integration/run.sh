#!/bin/sh
set -e

echo "=== Integration Test Suite ==="

# Setup: load test data
PGPASSWORD=postgres psql -h postgres -U postgres -d tiles_test -f /tests/setup.sql
echo "PASS: Database setup"

# Test 1: validate command
echo "--- Test: validate ---"
tilefeed -c /tests/config.toml validate
echo "PASS: validate"

# Test 2: generate command
echo "--- Test: generate ---"
tilefeed -c /tests/config.toml generate
echo "PASS: generate"

# Test 3: inspect command
echo "--- Test: inspect ---"
tilefeed inspect /tmp/test.mbtiles
echo "PASS: inspect"

# Test 4: diff command (same file = no changes)
echo "--- Test: diff ---"
cp /tmp/test.mbtiles /tmp/test_copy.mbtiles
tilefeed diff /tmp/test.mbtiles /tmp/test_copy.mbtiles
echo "PASS: diff"

echo ""
echo "=== All integration tests passed ==="
