#import "prelude.typ": conf
#show: conf.with(mode: "da", size: "small")

#align(center)[
  #image("images/da.webp", width: 50%)
]

#let input = json("input.json");
#for (n, player) in input.top.enumerate() {
  rect(stroke: white, width: 100%, inset: 10pt, radius: 10pt)[
    #(n + 1). #player.nickname
    #place(right + horizon)[#player.total_score]
    #place(right + horizon, dx: -64pt)[#(player.stars + player.at("hard_stars", default: 0)) x]
    #place(right + horizon, dx: -45pt)[#image("images/star-icon.png")]

    #if player.hard_score != 0 {
      place(right + top, dx: -95pt, dy: -2.5pt)[#text(size: 6pt)[#player.normal_score]]
      place(right + bottom, dx: -95pt, dy: 2.5pt)[#text(size: 6pt)[#player.hard_score]]
    }

  ]
}
