#import "prelude.typ": conf
#show: conf.with(mode: "diff", size: "big", group-digits: false)
#let data = json("input.json")
#set text(lang: "ru", font: "Maple Mono", size: 10pt)

#align(center)[
  #text(size: 18pt, data.agent) \
  #text(size: 9pt, data.revisions)
]

#let inline(s) = {
  for (i, part) in s.split("**").enumerate() {
    if calc.odd(i) { strong(text(part)) } else { text(part) }
  }
}
#let card(title, lines) = rect(width: 100%, radius: 10pt, inset: 12pt)[
  #if title != "" [
    #text(size: 12pt, baseline: 2pt, title)
    #line(length: 100%, stroke: 0.6pt + white)
  ]
  #for line in lines {
    if line.starts-with("- ") or line.starts-with("* ") {
      grid(
        columns: (4pt, 1fr),
        gutter: 5pt,
        [•], inline(line.slice(2)),
      )
    } else { block(inline(line)) }
  }
]
#{
  let title = ""
  let lines = ()
  for line in data.lines {
    if line.starts-with("#") {
      if lines.len() > 0 or title != "" { card(title, lines) }
      title = line.trim(regex("^#+ *"))
      lines = ()
    } else { lines.push(line) }
  }
  if lines.len() > 0 or title != "" { card(title, lines) }
}
