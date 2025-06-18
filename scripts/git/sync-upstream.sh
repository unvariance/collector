#!/bin/bash

set -e

echo "🔄 Syncing with upstream repository..."

# Check if upstream remote exists
if ! git remote get-url upstream >/dev/null 2>&1; then
    echo "❌ No 'upstream' remote found. Please add it first:"
    echo "   git remote add upstream <upstream-repo-url>"
    exit 1
fi

# Fetch from upstream
echo "📥 Fetching from upstream..."
git fetch upstream

# Get the main branch name
MAIN_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/origin/@@')
echo "📍 Main branch detected: $MAIN_BRANCH"

# Switch to main branch
echo "🔀 Switching to $MAIN_BRANCH"
git checkout "$MAIN_BRANCH"

# Merge upstream changes
echo "🔄 Merging upstream/$MAIN_BRANCH into local $MAIN_BRANCH"
git merge "upstream/$MAIN_BRANCH" --ff-only

# Push updates to origin (force push since we're syncing from upstream)
echo "📤 Pushing updates to origin"
git push origin "$MAIN_BRANCH" --force-with-lease

echo "✅ Successfully synced with upstream!" 