#!/bin/bash

set -e

BRANCH_NAME="$1"

if [ -z "$BRANCH_NAME" ]; then
    echo "Error: Branch name is required"
    exit 1
fi

echo "🌟 Creating new branch: $BRANCH_NAME"

# Ensure we're on main/master
MAIN_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/origin/@@')
echo "📍 Switching to $MAIN_BRANCH"
git checkout "$MAIN_BRANCH"

# Create and switch to new branch
echo "🔀 Creating and switching to branch: $BRANCH_NAME"
git checkout -b "$BRANCH_NAME"

echo "✅ Successfully created and switched to branch: $BRANCH_NAME" 