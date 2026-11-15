# loadbearer-fleet

## Reading files

- Never read `assets/app.js` whole. It is over 5,000 lines (about 243 KB). Search it
  with Grep for the function or text you need, then read only those lines with an
  offset and limit.
