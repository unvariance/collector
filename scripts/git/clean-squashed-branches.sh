#!/bin/bash

set -e

echo "🧹 Cleaning up squash-merged branches..."

# Get the main branch name  
MAIN_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/origin/@@')
echo "📍 Main branch: $MAIN_BRANCH"

# Switch to main branch
git checkout "$MAIN_BRANCH"

# Get list of local branches (excluding main/master)
LOCAL_BRANCHES=$(git branch | grep -v "\*\|$MAIN_BRANCH\|master\|main" | sed 's/^[ \t]*//')

if [ -z "$LOCAL_BRANCHES" ]; then
    echo "✨ No local branches to check"
    exit 0
fi

echo "🔍 Checking for squash-merged branches..."

for branch in $LOCAL_BRANCHES; do
    echo "Checking branch: $branch"
    
    # Get the merge-base between the branch and main
    MERGE_BASE=$(git merge-base "$branch" "$MAIN_BRANCH")
    
    # Get the tree of the branch
    BRANCH_TREE=$(git rev-parse "$branch^{tree}")
    
    # Check if there's a commit in main with the same tree as the branch
    # This indicates a squash merge
    if git rev-list --all --pretty=format:"%T %s" | grep "^$BRANCH_TREE" | grep -v "^commit" >/dev/null; then
        echo "🗑️  Branch '$branch' appears to be squash-merged, deleting..."
        git branch -D "$branch"
    else
        echo "📌 Branch '$branch' is not squash-merged, keeping..."
    fi
done

# Clean up remote tracking branches
echo "🔄 Pruning remote tracking branches..."
git remote prune origin

echo "✅ Squash-merged branch cleanup completed!" 