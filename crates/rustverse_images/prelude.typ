#let conf(mode: none, size: none, doc) = {
  set text(font: "inpin hongmengti", fill: white)
  set page(
    height: auto,
    width: if size == "big" { 20cm } else if size == "medium" { 12cm } else { 10cm },
    margin: 0.5cm,
    footer: align(right + horizon)[#text(size: 9pt, fill: white.transparentize(50%))[\@rustversebot]],
    fill: rgb(
      16,
      16,
      16,
    ),
    background: [

      #if mode == "da" {
        let grad1 = color.map.crest.map(x => x.transparentize(50%).darken(20%))
        let grad2 = color.map.flare.map(x => x.transparentize(50%).darken(20%))
        place(center + horizon)[#rect(
          stroke: none,
          width: 100%,
          height: 100%,
          fill: (gradient.linear(..grad1, ..grad1.rev()).repeat(1)),
        )]
        place(center + horizon)[#rect(
          stroke: none,
          width: 100%,
          height: 100%,
          fill: (gradient.linear(angle: 45deg, ..grad2, ..grad2.rev()).repeat(1)),
        )]
      }
      #if mode == "shiyu" {
        let grad1 = color.map.cividis.map(x => x.transparentize(50%).darken(20%))
        let grad2 = color.map.icefire.map(x => x.transparentize(80%).darken(20%))
        place(center + horizon)[#rect(
          stroke: none,
          width: 100%,
          height: 100%,
          fill: (gradient.linear(..grad1, ..grad1.rev()).repeat(1)),
        )]
        place(center + horizon)[#rect(
          stroke: none,
          width: 100%,
          height: 100%,
          fill: (gradient.linear(angle: 45deg, ..grad2, ..grad2.rev()).repeat(1)),
        )]
      }

    ],
  )

  set rect(fill: black.transparentize(80%), stroke: white, inset: 15pt)

  show regex("(?s)<color=#[0-9a-fA-F]{6}(?:[0-9a-fA-F]{2})?>.*?</color>"): it => {
    let m = it.text.match(regex("(?s)^<color=(#[0-9a-fA-F]{6}(?:[0-9a-fA-F]{2})?)>(.*)</color>$"))
    if m != none {
      text(fill: rgb(m.captures.at(0)))[#m.captures.at(1)]
    } else {
      it
    }
  }
  show " , ": ", "
  show "electric": "Electric"
  show "wind": "Wind"
  show "fire": "Fire"
  show "ice": "Ice"
  show "ether": "Ether"
  show "physical": "Physical"

  show regex("<IconMap.*?>"): ""
  show regex("<Term.*?>"): ""
  show regex("</Term>"): ""

  show regex("\d{7,}"): it => {
    text(str(it.text).rev().clusters().chunks(3).map(array.join).join(" ").rev())
  }

  doc
}
