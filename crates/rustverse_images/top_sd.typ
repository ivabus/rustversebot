#import "prelude.typ": conf
#show: conf.with(mode: "shiyu", size: "small")

#align(center)[
  #image("images/shiyu.webp", width: 30%)
]

#show "rating-B": text(fill: blue.lighten(50%))[B]
#show "rating-A": text(fill: purple.lighten(50%))[A]
#show "rating-S": text(fill: orange.lighten(50%))[S]
#show "rating-S+": text(fill: blue.lighten(50%))[S+]

#let input = json("input.json");
#for (n, player) in input.top.enumerate() {
  rect(stroke: white, width: 100%, inset: 10pt, radius: 10pt)[
    #(n + 1). #player.nickname
    #place(right + horizon)[#player.score]
    #place(center + horizon, dx: 48pt)[rating-#player.rating]


  ]
}
