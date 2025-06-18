#!/bin/bash

set -e

echo "🧹 Cleaning up merged branches..."

# Get the main branch name
MAIN_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/origin/@@')
echo "📍 Main branch: $MAIN_BRANCH"

# Switch to main branch
git checkout "$MAIN_BRANCH"

# Get current branch before cleanup
CURRENT_BRANCH=$(git branch --show-current)

echo "🔍 Looking for branches to clean up..."

# Delete local branches that have been merged (excluding main/master)
MERGED_BRANCHES=$(git branch --merged | grep -v "\*\|$MAIN_BRANCH\|master\|main" | xargs -n 1 echo)

if [ -z "$MERGED_BRANCHES" ]; then
    echo "✨ No merged branches to clean up"
else
    echo "🗑️  Deleting merged local branches:"
    echo "$MERGED_BRANCHES"
    echo "$MERGED_BRANCHES" | xargs -n 1 git branch -d
fi

# Clean up remote tracking branches that no longer exist on origin
echo "🔄 Pruning remote tracking branches..."
git remote prune origin

# List remaining branches
echo "📋 Remaining local branches:"
git branch

echo "✅ Branch cleanup completed!" 