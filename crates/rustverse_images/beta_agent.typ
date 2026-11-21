#import "prelude.typ": conf
#show: conf.with(mode: "diff", size: "big", group-digits: false)
#let data = json("input.json")
#set text(lang: "ru", font: "Maple Mono", size: 10pt)

#let inline(spans) = {
  for span in spans {
    let part = if span.code { text(fill: rgb("#bce3ef"), raw(span.text)) } else { text(span.text) }
    if span.strong { strong(part) } else { part }
  }
}

#align(center)[
  #text(size: 18pt, data.agent) \
  #text(size: 9pt, inline(data.revisions))
]

#let card(title, lines) = rect(width: 100%, radius: 10pt, inset: 12pt)[
  #if title.len() > 0 [
    #text(size: 12pt, baseline: 2pt, inline(title))
    #line(length: 100%, stroke: 0.6pt + white)
  ]
  #for line in lines {
    if line.kind == "bullet" {
      grid(
        columns: (4pt, 1fr),
        gutter: 5pt,
        [•], inline(line.spans),
      )
    } else { block(inline(line.spans)) }
  }
]
#{
  let title = ()
  let lines = ()
  for line in data.lines {
    if line.kind == "heading" {
      if lines.len() > 0 or title.len() > 0 { card(title, lines) }
      title = line.spans
      lines = ()
    } else { lines.push(line) }
  }
  if lines.len() > 0 or title.len() > 0 { card(title, lines) }
}
