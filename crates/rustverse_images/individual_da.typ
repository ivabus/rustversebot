#import "prelude.typ": conf

#show: conf.with(mode: "da", size: "medium")

#let input = json("input.json");


#align(center)[
  #input.nick_name
  #grid(
    columns: if input.has_hard { 2 } else { 1 },
    align: (right, left),
    inset: 3pt,
    [#input.total_score],
    if input.has_hard [#(
      input.hard_list.first().score
    )], [#(input.rank_percent / 100)%], if input.has_hard [#(input.hard_rank_percent / 100)%]
  )
]

#let room_view(room, is_hard: false) = [
  #rect(width: 100%, radius: 16pt, stroke: if is_hard { red } else { white })[
    #room.boss.first().name
    #place(right + top)[#room.score]

    #set text(size: 9pt)

    #columns(3)[
      #for avatar in room.avatar_list [
        #align(center + horizon)[
          #box(
            width: 50pt,
            height: 50pt,
            radius: 50%,
            clip: true,
            stroke: 1pt + white,
            image(
              "images/" + avatar.role_square_url.split("/").last(),
              width: 50pt,
            ),
          )
          #if avatar.rank != 0 and avatar.rarity != "A" [
            #place(center + top, dx: 25pt)[
              #avatar.rank
            ]
          ]
        ]
        #colbreak()
      ]
    ]
    #line(length: 100%, stroke: white)
    #room.buffer.first().title\
    #text(room.buffer.first().description.replace("\\n", "\n"))

  ]

]

#if input.has_hard [
  #room_view(input.hard_list.first(), is_hard: true)
]

#for room in input.list [
  #room_view(room)
]
