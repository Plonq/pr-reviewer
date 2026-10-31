# Project Overview

This is a tool for reviewing GitHub pull requests, tailored to a specific workflow. It is personal software, made by and for a single developer to streamline doing code reviews. Its architecture revolves around one flow: view pending review requests, and review them (view diff, make comments, submit review).

Its purpose is to improve upon the GitHub UI in the following ways. These points are the whole reason it was built:

1. Fast to navigate the PR/diff (switch files, etc)
2. Shows whole files, one at a time, providing the necessary context for a good review

The UI/UX design philosophy is:

- Optimised for both mouse and keyboard
- Only displays the information required, no more
- Only has the features that are needed, no more
- Clean, easy on the eyes, but information dense where it helps