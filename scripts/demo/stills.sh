#!/usr/bin/env bash
# The landing page's two stills: scripts/demo/stills.tape writes docs/assets/picker.png, then docs/assets/worktree.png
# is stacked from the tape's two pictures of the worktree. vhs writes a tape's Screenshots only when its
# recording ends, so the stacking runs here, after vhs, and never as a step of the tape. Needs what the
# tape needs (a built taw, vhs, JetBrainsMono Nerd Font) and ImageMagick (`magick`):
#
#   PATH=/opt/homebrew/bin:$PATH bash scripts/demo/stills.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
# The stack is made from this run's pictures only: a run that fails leaves none for magick to find.
rm -f dist/worktree-closed.png dist/worktree-open.png
vhs scripts/demo/stills.tape
# worktree.png: the two pictures stacked at the size they were taken (twice the size they are shown), closed row
# above and reopened session below, a thin line of the popup's border colour between them.
magick dist/worktree-closed.png \( dist/worktree-open.png -background '#45475a' -gravity north -splice 0x4 \) \
  -append -strip docs/assets/worktree.png
