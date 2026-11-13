#import "prelude.typ": conf

#show: conf.with(mode: "shiyu", size: "medium")

#let input = json("input.json");

// season info for boss names
#let season = json("season.json");


#align(center)[
  #input.nick_name
  #grid(
    columns: 1,
    align: (center),
    inset: 3pt,
    [#input.brief.score],
    [#input.brief.rating],
    [#(input.brief.rank_percent / 100)%],
  )
]

#let room_view(room, is_hard: false) = [
  #rect(width: 100%, radius: 16pt, stroke: if is_hard { red } else { white })[
    #place(right + top)[#room.score]

    #set text(size: 9pt)

    #(
      season
        .zone
        .at(str(room.layer_id))
        .layer_room
        .at(str(room.layer_id))
        .monster_list
        .values()
        .sorted(key: a => a.stats.at("hp"))
        .last()
        .name
    )

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
  ]

]

#for room in input.fitfh_layer_detail.layer_challenge_info_list [
  #room_view(room)
]
